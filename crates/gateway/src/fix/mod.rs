//! FIX connectivity via the QuickFIX C++ engine.
//!
//! One [`start_session`] call per broker/environment wires a full FIX order-entry
//! session into the OMS: outbound orders through [`adapter::FixBrokerAdapter`]
//! (registered in the [`BrokerRegistry`](crate::adapter::BrokerRegistry)), and
//! inbound execution reports through [`app::FixApplication`] to the
//! [order engine](crate::order_engine), which books them.
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
use tokio::time::Duration;
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
struct TracingFixLog;

impl LogCallback for TracingFixLog {
    fn on_incoming(&self, _session: Option<&SessionId>, msg: &str) {
        debug!(target: "fix::wire", dir = "in", "{}", redact_fix_secrets(msg).replace('\x01', "|"));
    }
    fn on_outgoing(&self, _session: Option<&SessionId>, msg: &str) {
        debug!(target: "fix::wire", dir = "out", "{}", redact_fix_secrets(msg).replace('\x01', "|"));
    }
    fn on_event(&self, _session: Option<&SessionId>, msg: &str) {
        info!(target: "fix::wire", "{}", msg);
    }
}

#[cfg(test)]
mod tracing_fix_log_tests {
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
use crate::contract::{EngineHandle, ExecEvent};
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
    default.set("ReconnectInterval", 5i32).map_err(cfg_err)?;
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

/// Start one FIX session: spawn the QuickFIX initiator (on its own OS thread) and
/// the async execution-report drain, returning the adapter to register in the
/// broker registry. `actor` labels the events this session appends (e.g. "ibkr").
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    dialect: Arc<dyn FixDialect>,
    cfg: FixConfig,
    actor: &'static str,
    health: StreamHandle,
    engine: EngineHandle,
    // Optional REST adapter that owns the reconciliation reads (open orders / order
    // status) for venues with no FIX equivalent (Binance Spot FIX). When present, the
    // recon driver reconciles through a SplitAdapter — orders over FIX, reads over
    // REST; when absent (IBKR) it uses the native 35=AF/35=H path.
    read_delegate: Option<Arc<dyn crate::adapter::BrokerAdapter>>,
) -> Result<Arc<FixBrokerAdapter>, BrokerError> {
    let begin_string = dialect.begin_string();
    // Validate settings eagerly so a misconfiguration surfaces at startup; the
    // real SessionSettings is rebuilt inside the session thread because its FFI
    // handle is not Send.
    build_settings(&cfg, begin_string)?;

    let pending = Arc::new(Mutex::new(HashMap::new()));
    let snapshots = Arc::new(Mutex::new(HashMap::new()));
    let statuses = Arc::new(Mutex::new(HashMap::new()));
    let (report_tx, mut report_rx) = mpsc::unbounded_channel();
    // Logon nudges (bounded, coalescing) for the order-reconcile driver.
    let (logon_tx, mut logon_rx) = mpsc::channel::<()>(4);

    // Venue/registry code (e.g. "IBKR"), captured before `dialect` moves onto the
    // session thread — used as the broker_code for reconciliation queries.
    let broker_code = dialect.venue();

    // Forward inbound execution reports to the order engine, which books them.
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            while let Some((order_id, report)) = report_rx.recv().await {
                engine.exec(ExecEvent::from_report(order_id, broker_code, actor, report)).await;
            }
        });
    }

    let adapter = Arc::new(FixBrokerAdapter::new(
        dialect.clone(),
        pending.clone(),
        snapshots.clone(),
        statuses.clone(),
        cfg.sender_comp_id.clone(),
        cfg.target_comp_id.clone(),
    ));

    // The adapter the recon driver reconciles through: pure FIX by default, or a
    // FIX-writer / REST-reader split when a read delegate is supplied. Order routing
    // (the returned `adapter`) always stays pure FIX.
    let recon_adapter: Arc<dyn crate::adapter::BrokerAdapter> = match read_delegate {
        Some(reader) => Arc::new(crate::adapter::SplitAdapter { writer: adapter.clone(), reader }),
        None => adapter.clone(),
    };

    // Order-reconcile driver: mass-status snapshot on each (re)logon and every 30s,
    // diffing the broker's open orders against the OMS working set. This is FIX's
    // snapshot-on-reconnect — necessary because `ResetOnLogon=Y` disables QuickFIX's
    // own resend of anything missed during an outage.
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // consume the immediate first tick
            loop {
                tokio::select! {
                    recv = logon_rx.recv() => {
                        if recv.is_none() {
                            break; // session gone — stop the driver
                        }
                    }
                    _ = interval.tick() => {}
                }
                engine.reconcile(broker_code, actor, recon_adapter.clone()).await;
            }
        });
    }

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
            let app_cb =
                FixApplication::new(dialect, health, pending, snapshots, statuses, report_tx, logon_tx);
            let application = match Application::try_new(&app_cb) {
                Ok(a) => a,
                Err(e) => { error!(venue, error = %e, "FIX: application init failed"); return; }
            };
            let store = MemoryMessageStoreFactory::new();
            let log = match LogFactory::try_new(&TracingFixLog) {
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

    Ok(adapter)
}
