//! FIX connectivity via the QuickFIX C++ engine.
//!
//! One [`start_session`] call per broker/environment wires a full FIX order-entry
//! session into the OMS: outbound orders through [`adapter::FixBrokerAdapter`]
//! (registered in the [`BrokerRegistry`](crate::adapters::BrokerRegistry)), and
//! inbound execution reports through [`app::FixApplication`] into the existing
//! [`process_execution_report`](crate::execution::process_execution_report) path.
//! QuickFIX owns the session mechanics — logon, heartbeat, sequence numbers,
//! resend/gap-fill, TLS, and reconnect — so there is no supervisor loop here.

pub mod adapter;
pub mod app;
pub mod binance;
pub mod dialect;
pub mod ibkr;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use quickfix::{
    Application, ConnectionHandler, Dictionary, FixSocketServerKind, Initiator, LogCallback,
    LogFactory, MemoryMessageStoreFactory, SessionId, SessionSettings,
};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{error, info};

/// Routes QuickFIX's session log (raw messages + admin events) into `tracing` so
/// the FIX wire is visible alongside the rest of the OMS logs. SOH is rendered as
/// `|`. Messages log at debug; session events at info.
struct TracingFixLog;

impl LogCallback for TracingFixLog {
    fn on_incoming(&self, _session: Option<&SessionId>, msg: &str) {
        info!(target: "fix::wire", dir = "in", "{}", msg.replace('\x01', "|"));
    }
    fn on_outgoing(&self, _session: Option<&SessionId>, msg: &str) {
        info!(target: "fix::wire", dir = "out", "{}", msg.replace('\x01', "|"));
    }
    fn on_event(&self, _session: Option<&SessionId>, msg: &str) {
        info!(target: "fix::wire", "{}", msg);
    }
}

use crate::adapters::BrokerError;
use crate::execution::process_execution_report;
use crate::fix::adapter::FixBrokerAdapter;
use crate::fix::app::FixApplication;
use crate::fix::binance::BinanceDialect;
use crate::fix::dialect::FixDialect;
use crate::fix::ibkr::IbkrDialect;
use crate::kafka::KafkaClient;
use crate::recon_orders::run_order_reconcile;
use crate::stream_health::{StreamHandle, StreamHealthRegistry};

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
    pool: PgPool,
    kafka: Option<KafkaClient>,
    position_changed_tx: Option<mpsc::Sender<()>>,
    // Optional REST adapter that owns the reconciliation reads (open orders / order
    // status) for venues with no FIX equivalent (Binance Spot FIX). When present, the
    // recon driver reconciles through a SplitAdapter — orders over FIX, reads over
    // REST; when absent (IBKR) it uses the native 35=AF/35=H path.
    read_delegate: Option<Arc<dyn crate::adapters::BrokerAdapter>>,
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

    // Drain inbound execution reports into the shared apply path.
    {
        let pool = pool.clone();
        let kafka = kafka.clone();
        let position_changed_tx = position_changed_tx.clone();
        tokio::spawn(async move {
            while let Some((order_id, report)) = report_rx.recv().await {
                if let Err(e) = process_execution_report(
                    &pool,
                    &kafka,
                    order_id,
                    report,
                    actor,
                    position_changed_tx.as_ref(),
                )
                .await
                {
                    error!(order_id = %order_id, error = %e, "FIX: failed to apply execution report");
                }
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
    let recon_adapter: Arc<dyn crate::adapters::BrokerAdapter> = match read_delegate {
        Some(reader) => Arc::new(crate::adapters::SplitAdapter { writer: adapter.clone(), reader }),
        None => adapter.clone(),
    };

    // Order-reconcile driver: mass-status snapshot on each (re)logon and every 30s,
    // diffing the broker's open orders against the OMS working set. This is FIX's
    // snapshot-on-reconnect — necessary because `ResetOnLogon=Y` disables QuickFIX's
    // own resend of anything missed during an outage.
    {
        let pool = pool.clone();
        let kafka = kafka.clone();
        let position_changed_tx = position_changed_tx.clone();
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
                run_order_reconcile(
                    &pool,
                    &kafka,
                    &*recon_adapter,
                    broker_code,
                    actor,
                    position_changed_tx.as_ref(),
                )
                .await;
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

/// Read `{prefix}_{suffix}`, returning `None` when unset or empty.
fn env_opt(prefix: &str, suffix: &str) -> Option<String> {
    std::env::var(format!("{prefix}_{suffix}")).ok().filter(|s| !s.is_empty())
}

/// Wire an IBKR FIX order-entry session for `env_name` if `IBKR_{ENV}_FIX_HOST` is
/// set. Returns the adapter to register in the broker registry, or `None` when not
/// configured. Requires `_FIX_PORT`, `_SENDER_COMP_ID`, `_TARGET_COMP_ID`,
/// `_FIX_PASSWORD`.
#[allow(clippy::too_many_arguments)]
pub fn start_ibkr(
    env_name: &str,
    stream_health: &StreamHealthRegistry,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    position_changed_tx: Option<mpsc::Sender<()>>,
) -> Option<Arc<FixBrokerAdapter>> {
    let prefix = format!("IBKR_{env_name}");
    let host = env_opt(&prefix, "FIX_HOST")?;
    let cfg = FixConfig {
        host,
        port: env_opt(&prefix, "FIX_PORT").and_then(|s| s.parse().ok()).unwrap_or(4001),
        sender_comp_id: env_opt(&prefix, "SENDER_COMP_ID").unwrap_or_else(|| "OMS".into()),
        target_comp_id: env_opt(&prefix, "TARGET_COMP_ID").unwrap_or_else(|| "IBKR".into()),
        ssl: env_opt(&prefix, "FIX_SSL").map(|s| s != "N").unwrap_or(true),
        heartbeat_secs: 30,
    };
    // Seed the health entry only now that we know the session is configured.
    let health = stream_health.fix_handle("IBKR", env_name);
    let password = env_opt(&prefix, "FIX_PASSWORD").unwrap_or_default();
    let dialect: Arc<dyn FixDialect> = Arc::new(IbkrDialect::new(password));
    // IBKR uses the native FIX 35=AF/35=H recon path (no REST delegate).
    match start_session(dialect, cfg, "ibkr", health, pool, kafka, position_changed_tx, None) {
        Ok(a) => { info!(env = env_name, "registered IBKR FIX adapter"); Some(a) }
        Err(e) => { error!(env = env_name, error = %e, "IBKR FIX session not started"); None }
    }
}

/// Wire a Binance Spot FIX order-entry session for `env_name` if
/// `BINANCE_{ENV}_FIX_HOST` is set. Reuses the existing `BINANCE_{ENV}_API_KEY` +
/// `_PRIVATE_KEY_PATH` credential for Ed25519 logon signing.
#[allow(clippy::too_many_arguments)]
pub fn start_binance(
    env_name: &str,
    stream_health: &StreamHealthRegistry,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    position_changed_tx: Option<mpsc::Sender<()>>,
) -> Option<Arc<FixBrokerAdapter>> {
    let prefix = format!("BINANCE_{env_name}");
    let host = env_opt(&prefix, "FIX_HOST")?;
    let api_key = env_opt(&prefix, "API_KEY")?;
    let pem_path = env_opt(&prefix, "PRIVATE_KEY_PATH")?;
    let pem = match std::fs::read_to_string(&pem_path) {
        Ok(p) => p,
        Err(e) => { error!(env = env_name, "Binance FIX: cannot read {pem_path}: {e}"); return None; }
    };
    let dialect = match BinanceDialect::new(api_key.clone(), &pem) {
        Ok(d) => Arc::new(d) as Arc<dyn FixDialect>,
        Err(e) => { error!(env = env_name, error = %e, "Binance FIX dialect init failed"); return None; }
    };
    // Binance Spot FIX has no open-orders / order-status message, so route the
    // order-reconciliation reads through the same credential's REST API. Orders still
    // go over FIX; only the recon snapshot/status reads use REST.
    let read_delegate: Option<Arc<dyn crate::adapters::BrokerAdapter>> =
        match crate::adapters::binance::BinanceAdapter::new(api_key, &pem, env_name) {
            Ok(a) => Some(Arc::new(a)),
            Err(e) => {
                error!(env = env_name, error = %e, "Binance FIX: REST recon delegate init failed; recon disabled");
                None
            }
        };
    let cfg = FixConfig {
        host,
        port: env_opt(&prefix, "FIX_PORT").and_then(|s| s.parse().ok()).unwrap_or(9000),
        sender_comp_id: env_opt(&prefix, "SENDER_COMP_ID").unwrap_or_else(|| "OMS".into()),
        target_comp_id: env_opt(&prefix, "TARGET_COMP_ID").unwrap_or_else(|| "SPOT".into()),
        ssl: true, // Binance FIX requires TLS.
        heartbeat_secs: 30,
    };
    // Seed the health entry only now that we know the session is configured.
    let health = stream_health.fix_handle("BINANCE", env_name);
    match start_session(dialect, cfg, "binance", health, pool, kafka, position_changed_tx, read_delegate) {
        Ok(a) => { info!(env = env_name, "registered BINANCE FIX adapter"); Some(a) }
        Err(e) => { error!(env = env_name, error = %e, "Binance FIX session not started"); None }
    }
}
