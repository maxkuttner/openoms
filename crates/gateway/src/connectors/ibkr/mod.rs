pub mod fix;

use futures_util::future::BoxFuture;

use crate::verify::TestOutcome;
use crate::credentials::BrokerCredentials;
use crate::connectors::RegistrationDeps;
use crate::connectors::ibkr::fix::{IbkrDialect, IbkrFixCredentials};
use std::sync::Arc;

use tracing::{error, info};

use crate::fix::adapter::FixBrokerAdapter;
use crate::fix::dialect::FixDialect;
use crate::fix::{start_session, FixConfig};
use crate::contract::EngineHandle;
use crate::stream_health::StreamHealthRegistry;

use super::Connected;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Input, Venue, DOCS};

use super::{FIX_NOT_TESTABLE, Connector, FieldType, Manifest, QuoteScope};

pub struct Ibkr;

static MANIFEST: Manifest = Manifest {
    code: "IBKR",
    credential_kind: "IbkrFix",
    venue,
    field_types: &[("port", FieldType::Port), ("ssl", FieldType::Bool { default: true })],
    redaction: &[("host", Clear), ("port", Clear), ("sender_comp_id", Clear), ("target_comp_id", Clear), ("ssl", Clear), ("password", Hidden)],
    live_only: false,
    universe: false,
    quotes: QuoteScope::None,
    exec_stream: false,
    session_bound: true,
};

impl Connector for Ibkr {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// FIX-only: the session both routes orders and delivers execution reports.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String> {
        let adapter = start_ibkr(env, creds, &deps.stream_health, deps.engine.clone())
            .ok_or_else(|| "IBKR FIX session could not start".to_string())?;
        Ok(Connected { adapter, exec_stream: None, daily_stats: None })
    }

    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { FIX_NOT_TESTABLE })
    }
}

fn venue() -> Venue {
    Venue {
        code: "IBKR",
        name: "Interactive Brokers",
        description: "Multi-asset over FIX; experimental, no instrument loading",
        docs_url: format!("{DOCS}/interactive-brokers/"),
        universe: None,
        environments: environments("paper account", None, None),
        fields: vec![
            field("password", "Password").secret(),
            field("host", "Host").fix(),
            field("port", "Port").fix(),
            field("sender_comp_id", "SenderCompID").fix(),
            field("target_comp_id", "TargetCompID").fix().prefill("IBKR", "IBKR"),
            field("ssl", "TLS").fix().input(Input::Checkbox).prefill("true", "true"),
        ],
    }
}

/// Wire an IBKR FIX order-entry session for `env_name` from an already-decrypted
/// credential (see `crate::credentials`). Returns the adapter to register in the
/// broker registry, or `None` if the session failed to start.
///
/// `creds` must be an `IbkrFix` credential — the caller (`serve()`) only
/// reaches this function after matching that variant out of the store, so a
/// mismatch here means a caller bug, not bad operator input; it is logged and
/// treated as "not started" rather than panicking, so one broken caller cannot
/// take the process down.
fn start_ibkr(
    env_name: &str,
    creds: &crate::credentials::BrokerCredentials,
    stream_health: &StreamHealthRegistry,
    engine: EngineHandle,
) -> Option<Arc<FixBrokerAdapter>> {
    let parsed = match creds.kind.as_str() {
        IbkrFixCredentials::KIND => creds.parse::<IbkrFixCredentials>().ok(),
        _ => None,
    };
    let Some(parsed) = parsed else {
        error!(env = env_name, "start_ibkr called with non-IBKR credentials (programming error)");
        return None;
    };
    let IbkrFixCredentials { host, port, sender_comp_id, target_comp_id, password, ssl } = &parsed;
    let cfg = FixConfig {
        host: host.clone(),
        port: *port,
        sender_comp_id: sender_comp_id.clone(),
        target_comp_id: target_comp_id.clone(),
        ssl: *ssl,
        heartbeat_secs: 30,
    };
    // Seed the health entry only now that we know the session is configured.
    let health = stream_health.fix_handle("IBKR", env_name);
    let dialect: Arc<dyn FixDialect> = Arc::new(IbkrDialect::new(password.clone()));
    // IBKR uses the native FIX 35=AF/35=H recon path (no REST delegate).
    match start_session(dialect, cfg, "ibkr", health, engine, None) {
        Ok(a) => { info!(env = env_name, "registered IBKR FIX adapter"); Some(a) }
        Err(e) => { error!(env = env_name, error = %e, "IBKR FIX session not started"); None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine nothing drains — safe here because every test below hits a
    /// guard that returns `None` before the network or QuickFIX are touched.
    fn idle_engine() -> EngineHandle {
        EngineHandle::new().0
    }

    fn alpaca_cred() -> BrokerCredentials {
        BrokerCredentials::from_typed("Alpaca", &crate::connectors::alpaca::adapter::AlpacaCredentials { key: "k".into(), secret: "s".into() })
    }


    /// A caller bug (the wrong credential variant reaching `start_ibkr`) must be
    /// logged and refused, not panic or attempt a connection built from garbage
    /// data — see the doc comment on `start_ibkr`. `#[tokio::test]`, not
    /// `#[test]`: kept on a Tokio runtime like the session it guards.
    #[tokio::test]
    async fn start_ibkr_refuses_a_non_ibkr_credential() {
        let health = StreamHealthRegistry::new();
        let result = start_ibkr("PAPER", &alpaca_cred(), &health, idle_engine());
        assert!(result.is_none());
    }
}
