//! FIX connectivity via the QuickFIX C++ engine.
//!
//! One [`start_session`] call per broker/environment wires a full FIX order-entry
//! session into the OMS: outbound orders through [`adapter::FixBrokerAdapter`]
//! (registered in the [`BrokerRegistry`](crate::adapter::BrokerRegistry)), and
//! inbound execution reports through [`app::FixApplication`] as venue events,
//! out of the [`VenueStream`](crate::connectors::VenueStream) the session
//! returns: the OMS feeds them to the connection's reconciler (with a pull
//! source) or straight to ingest (without one).
//! QuickFIX owns the session mechanics — logon, heartbeat, sequence numbers,
//! resend/gap-fill, TLS, and reconnect — so there is no supervisor loop here.

pub mod adapter;
pub mod app;
pub mod dialect;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quickfix::{
    Application, ConnectionHandler, Dictionary, FixSocketServerKind, Initiator, LogCallback,
    LogFactory, MemoryMessageStoreFactory, SessionId, SessionSettings,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// FIX tags whose value is authentication material and must never reach a log
/// line: 553 (Username — the Binance API key, `binance.rs`), 554 (Password —
/// the IBKR account password, `ibkr.rs`), and 96 (RawData — the Binance Ed25519
/// logon signature). Every other tag is left alone: the wire log is a real
/// debugging tool and must stay useful.
const SECRET_FIX_TAGS: [&str; 3] = ["553", "554", "96"];

/// Redact the value of every [`SECRET_FIX_TAGS`] field in a raw, SOH-delimited
/// FIX message. Splitting on SOH first (rather than a substring search for
/// `"554="` etc.) is what keeps this from mangling a value that merely
/// *contains* those characters elsewhere in the message — only a field whose
/// own tag matches is touched.
fn redact_fix_secrets(msg: &str) -> String {
    msg.split('\x01')
        .map(|field| match field.split_once('=') {
            Some((tag, _)) if SECRET_FIX_TAGS.contains(&tag) => format!("{tag}=<redacted>"),
            _ => field.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\x01")
}

/// Routes QuickFIX's session log (raw messages + admin events) into `tracing` so
/// the FIX wire is visible alongside the rest of the OMS logs. SOH is rendered as
/// `|`. Messages log at debug under the `fix::wire` target (with `main.rs`'s
/// default `EnvFilter` of `info`, this keeps the wire off stdout unless
/// `RUST_LOG=fix::wire=debug` is set, which shows it); session events at info.
/// Secret tags are redacted in both directions regardless of level — see
/// `redact_fix_secrets`.
pub(crate) struct TracingFixLog;

impl LogCallback for TracingFixLog {
    fn on_incoming(&self, _session: Option<&SessionId>, msg: &str) {
        debug!(target: "fix::wire", dir = "in", "{}", redact_fix_secrets(msg).replace('\x01', "|"));
    }
    fn on_outgoing(&self, _session: Option<&SessionId>, msg: &str) {
        debug!(target: "fix::wire", dir = "out", "{}", redact_fix_secrets(msg).replace('\x01', "|"));
    }
    fn on_event(&self, _session: Option<&SessionId>, msg: &str) {
        info!(target: "fix::wire", "{}", event_line(msg));
    }
}

/// What a QuickFIX session event says about the connection's health.
#[derive(Debug, PartialEq)]
pub(crate) enum FixLogHealth {
    /// A connect attempt started ("connecting to <host> on port <port>").
    Attempt(String),
    /// The attempt or the session failed; QuickFIX retries on its ReconnectInterval.
    Failed(String),
}

/// Reads a QuickFIX session event for health. QuickFIX reports connect
/// attempts and their failures only through its log, not the application.
pub(crate) fn fix_log_health(msg: &str) -> Option<FixLogHealth> {
    if let Some(rest) = msg.strip_prefix("Connecting to ") {
        let target = rest.split(" (Source").next().unwrap_or(rest);
        return Some(FixLogHealth::Attempt(format!("connecting to {target}")));
    }
    let failed = msg == "Disconnecting"
        || msg.starts_with("Timed out waiting for logon")
        || msg.starts_with("SSL_connect failed")
        || msg.starts_with("Failed to create ssl")
        || msg.starts_with("BIO_new_socket failed");
    failed.then(|| FixLogHealth::Failed(msg.to_string()))
}

/// Seconds QuickFIX waits between connect attempts (its `ReconnectInterval`).
pub(crate) const RECONNECT_INTERVAL_SECS: u64 = 5;

/// [`TracingFixLog`] for an initiator session that also reports its connect
/// attempts to the session's health: QuickFIX tells the application about a
/// logon and a logout, but a refused or timed-out attempt only reaches the log.
struct SessionHealthLog {
    health: StreamHandle,
}

impl LogCallback for SessionHealthLog {
    fn on_incoming(&self, session: Option<&SessionId>, msg: &str) {
        TracingFixLog.on_incoming(session, msg);
    }
    fn on_outgoing(&self, session: Option<&SessionId>, msg: &str) {
        TracingFixLog.on_outgoing(session, msg);
    }
    fn on_event(&self, session: Option<&SessionId>, msg: &str) {
        TracingFixLog.on_event(session, msg);
        match fix_log_health(msg) {
            Some(FixLogHealth::Attempt(m)) => self.health.set_connecting_because(m),
            Some(FixLogHealth::Failed(m)) => self
                .health
                .set_down_retrying(event_line(&m), std::time::Duration::from_secs(RECONNECT_INTERVAL_SECS)),
            None => {}
        }
    }
}

/// A session event as logged. QuickFIX quotes whole messages in some events
/// ("Session not found for incoming message: …"), so secrets are redacted here too.
fn event_line(msg: &str) -> String {
    redact_fix_secrets(msg).replace('\x01', "|")
}

#[cfg(test)]
mod session_health_log_tests {
    use super::{fix_log_health, FixLogHealth};

    #[test]
    fn a_connect_attempt_is_connecting_and_a_failed_one_is_down() {
        assert_eq!(
            fix_log_health("Connecting to fix.example.com on port 4001 (Source :0) ReconnectInterval=5"),
            Some(FixLogHealth::Attempt("connecting to fix.example.com on port 4001".into()))
        );
        for failed in ["Disconnecting", "Timed out waiting for logon response", "SSL_connect failed with SSL error 1"] {
            assert_eq!(fix_log_health(failed), Some(FixLogHealth::Failed(failed.into())), "{failed}");
        }
        assert_eq!(fix_log_health("Initiated logon request"), None);
    }
}

#[cfg(test)]
mod tracing_fix_log_tests {
    #[test]
    fn an_event_quoting_a_logon_does_not_log_its_password() {
        let line = super::event_line("Session not found for incoming message: 8=FIX.4.4\x0135=A\x01554=s3cret\x0110=195\x01");
        assert!(!line.contains("s3cret"), "{line}");
        assert!(line.contains("554="), "{line}");
    }

    use super::redact_fix_secrets;

    /// The IBKR logon: tag 554 carries the account password. Everything else
    /// (message type, sender, target) must survive so the log stays useful.
    #[test]
    fn redacts_ibkr_password_and_keeps_other_tags() {
        let msg = "8=FIX.4.2\x019=70\x0135=A\x0149=OMS\x0156=IBKR\x01554=MYPASSWORD\x0110=001\x01";
        let out = redact_fix_secrets(msg);
        assert!(!out.contains("MYPASSWORD"), "password leaked: {out}");
        assert!(out.contains("554=<redacted>"), "tag 554 should show as redacted: {out}");
        assert!(out.contains("35=A"), "message type must survive: {out}");
        assert!(out.contains("49=OMS"), "sender comp id must survive: {out}");
    }

    /// The Binance logon: tag 553 carries the API key, tag 96 the Ed25519
    /// signature. Both must be gone; the rest of the logon stays intact.
    #[test]
    fn redacts_binance_api_key_and_signature() {
        let msg = "8=FIX.4.4\x019=90\x0135=A\x0149=OMS\x0156=SPOT\x01553=MYAPIKEY\x0196=SIGNATUREBASE64\x0110=002\x01";
        let out = redact_fix_secrets(msg);
        assert!(!out.contains("MYAPIKEY"), "api key leaked: {out}");
        assert!(!out.contains("SIGNATUREBASE64"), "signature leaked: {out}");
        assert!(out.contains("553=<redacted>"), "tag 553 should show as redacted: {out}");
        assert!(out.contains("96=<redacted>"), "tag 96 should show as redacted: {out}");
        assert!(out.contains("35=A"));
        assert!(out.contains("49=OMS"));
    }

    /// A value that merely *contains* "554=" as text — not the tag itself —
    /// must not be mangled. Splitting on SOH field boundaries is what makes
    /// this safe: only a field whose own tag is exactly "554" is touched.
    #[test]
    fn does_not_mangle_a_substring_that_merely_looks_like_a_secret_tag() {
        let msg = "35=A\x0149=OMS\x0158=note: contains 554=999 as literal text\x0110=003\x01";
        let out = redact_fix_secrets(msg);
        assert!(
            out.contains("58=note: contains 554=999 as literal text"),
            "unrelated field must survive untouched: {out}"
        );
    }
}

use crate::adapter::BrokerError;
use crate::fix::adapter::FixBrokerAdapter;
use crate::fix::app::FixApplication;
use crate::fix::dialect::FixDialect;
use crate::connectors::VenueStream;
use crate::stream_health::StreamHandle;

/// Connection parameters for one FIX session. Credentials live on the dialect;
/// this is only the endpoint + identity.
pub struct FixConfig {
    pub host: String,
    pub port: u16,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    /// TLS to the broker (required by Binance; typically true for IBKR too).
    pub ssl: bool,
    pub heartbeat_secs: u32,
}

/// Build the QuickFIX `SessionSettings` for a single initiator session.
fn build_settings(cfg: &FixConfig, begin_string: &str) -> Result<(SessionSettings, SessionId), BrokerError> {
    let cfg_err = |e: quickfix::QuickFixError| BrokerError::NotConfigured(format!("FIX settings: {e}"));

    let mut settings = SessionSettings::new();

    let mut default = Dictionary::new();
    default.set("ConnectionType", "initiator".to_string()).map_err(cfg_err)?;
    default.set("ReconnectInterval", RECONNECT_INTERVAL_SECS as i32).map_err(cfg_err)?;
    default.set("HeartBtInt", cfg.heartbeat_secs as i32).map_err(cfg_err)?;
    default.set("SocketConnectHost", cfg.host.clone()).map_err(cfg_err)?;
    default.set("SocketConnectPort", cfg.port as i32).map_err(cfg_err)?;
    // 24h session (crypto never closes; IBKR schedules are handled broker-side).
    default.set("StartTime", "00:00:00".to_string()).map_err(cfg_err)?;
    default.set("EndTime", "00:00:00".to_string()).map_err(cfg_err)?;
    // Broker dialects carry custom tags standard dictionaries reject.
    default.set("UseDataDictionary", "N".to_string()).map_err(cfg_err)?;
    // Reset sequence numbers on each logon so an in-memory store is correct across
    // restarts (Binance requires the reset flag anyway).
    default.set("ResetOnLogon", "Y".to_string()).map_err(cfg_err)?;
    settings.set(None, default).map_err(cfg_err)?;

    let session_id = SessionId::try_new(begin_string, &cfg.sender_comp_id, &cfg.target_comp_id, "")
        .map_err(cfg_err)?;
    let mut session = Dictionary::new();
    session.set("BeginString", begin_string.to_string()).map_err(cfg_err)?;
    session.set("SenderCompID", cfg.sender_comp_id.clone()).map_err(cfg_err)?;
    session.set("TargetCompID", cfg.target_comp_id.clone()).map_err(cfg_err)?;
    settings.set(Some(&session_id), session).map_err(cfg_err)?;

    Ok((settings, session_id))
}

/// Start one FIX session: spawn the QuickFIX initiator (on its own OS thread),
/// returning the adapter to register and the session's venue-event stream.
///
/// The session has no stream task of its own — the QuickFIX callbacks emit the
/// events. The returned [`VenueStream`] drains them into the sink it is given
/// until that sink closes; the receiver outlives an aborted drain (it sits
/// behind a shared lock), so a restarted stream picks up where the last left off.
pub fn start_session(
    dialect: Arc<dyn FixDialect>,
    cfg: FixConfig,
    health: StreamHandle,
) -> Result<(Arc<FixBrokerAdapter>, VenueStream), BrokerError> {
    let begin_string = dialect.begin_string();
    // Validate settings eagerly so a misconfiguration surfaces at startup; the
    // real SessionSettings is rebuilt inside the session thread because its FFI
    // handle is not Send.
    build_settings(&cfg, begin_string)?;
    // QuickFIX answers our HeartBtInt with a Heartbeat (or a TestRequest) each interval.
    health.set_heartbeat_secs(u64::from(cfg.heartbeat_secs));

    let pending = Arc::new(Mutex::new(HashMap::new()));
    let snapshots = Arc::new(Mutex::new(HashMap::new()));
    let statuses = Arc::new(Mutex::new(HashMap::new()));
    let (venue_tx, venue_rx) = mpsc::unbounded_channel();
    let venue_rx = Arc::new(tokio::sync::Mutex::new(venue_rx));
    let venue_stream: VenueStream = Arc::new(move |_health, sink| {
        let rx = venue_rx.clone();
        Box::pin(async move {
            let mut rx = rx.lock().await;
            while let Some(event) = rx.recv().await {
                if sink.send(event).await.is_err() {
                    break;
                }
            }
        })
    });

    let adapter = Arc::new(FixBrokerAdapter::new(
        dialect.clone(),
        pending.clone(),
        snapshots.clone(),
        statuses.clone(),
        cfg.sender_comp_id.clone(),
        cfg.target_comp_id.clone(),
    ));

    let server_kind = if cfg.ssl {
        FixSocketServerKind::SslSingleThreaded
    } else {
        FixSocketServerKind::SingleThreaded
    };
    let venue = dialect.venue();

    // QuickFIX runs its own threads and reconnects on its own. The SessionSettings,
    // Application, store, log factory, and Initiator all live on a dedicated OS
    // thread that owns them and parks forever (none of these FFI handles are Send).
    std::thread::Builder::new()
        .name(format!("fix-{}", venue.to_lowercase()))
        .spawn(move || {
            let settings = match build_settings(&cfg, begin_string) {
                Ok((s, _)) => s,
                Err(e) => { error!(venue, error = %e, "FIX: settings build failed"); return; }
            };
            let session_log = SessionHealthLog { health: health.clone() };
            let app_cb =
                FixApplication::new(dialect, health, pending, snapshots, statuses, venue_tx);
            let application = match Application::try_new(&app_cb) {
                Ok(a) => a,
                Err(e) => { error!(venue, error = %e, "FIX: application init failed"); return; }
            };
            let store = MemoryMessageStoreFactory::new();
            let log = match LogFactory::try_new(&session_log) {
                Ok(l) => l,
                Err(e) => { error!(venue, error = %e, "FIX: log factory init failed"); return; }
            };
            let mut initiator = match Initiator::try_new(&settings, &application, &store, &log, server_kind) {
                Ok(i) => i,
                Err(e) => { error!(venue, error = %e, "FIX: initiator init failed"); return; }
            };
            if let Err(e) = initiator.start() {
                error!(venue, error = %e, "FIX: initiator start failed");
                return;
            }
            info!(venue, "FIX initiator started");
            loop {
                std::thread::park();
            }
        })
        .map_err(|e| BrokerError::NotConfigured(format!("FIX thread spawn: {e}")))?;

    Ok((adapter, venue_stream))
}
