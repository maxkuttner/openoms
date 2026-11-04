pub mod adapter;
pub mod stream;
pub mod feed;
pub mod fix;

use futures_util::future::BoxFuture;

use crate::verify::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::connectors::RegistrationDeps;
use crate::connectors::binance::fix::BinanceDialect;
use tracing::{error, info};

use crate::fix::adapter::FixBrokerAdapter;
use crate::fix::dialect::FixDialect;
use crate::fix::{start_session, FixConfig};
use crate::contract::EngineHandle;
use crate::stream_health::StreamHealthRegistry;

use super::{FIX_NOT_TESTABLE, PublicFeed, typed, Connected};
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Input, Venue, DOCS};

use super::{Connector, FieldType, Manifest, QuoteScope};

pub struct Binance;

static MANIFEST: Manifest = Manifest {
    code: "BINANCE",
    credential_kind: "BinanceFix",
    venue,
    field_types: &[("port", FieldType::Port)],
    redaction: &[("host", Clear), ("port", Clear), ("sender_comp_id", Clear), ("target_comp_id", Clear), ("api_key", Hidden), ("private_key", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
    exec_stream: true,
    session_bound: true,
};

impl Connector for Binance {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// Transport is an explicit choice via `BINANCE_{ENV}_TRANSPORT=fix|rest`
    /// (default rest) — a wire-protocol setting, not a secret, so it stays on the
    /// environment. `fix` runs one FIX session for order entry and execution
    /// reports; `rest` runs the REST adapter plus the WS user-data stream, built
    /// from the same stored credential.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String> {
        use crate::adapter::Transport;
        match Transport::from_env(&format!("BINANCE_{env}"), Transport::Rest) {
            Transport::Fix => {
                let adapter = start_binance(env, creds, &deps.stream_health, deps.engine.clone())
                    .ok_or_else(|| "Binance FIX session could not start".to_string())?;
                Ok(Connected { adapter, exec_stream: None, daily_stats: None })
            }
            Transport::Rest => {
                let c: crate::connectors::binance::adapter::BinanceCredentials = typed(creds, MANIFEST.credential_kind)?;
                let adapter = crate::connectors::binance::adapter::BinanceAdapter::new(c.api_key, &c.private_key, env)
                    .map_err(|e| format!("Binance adapter not registered: {e}"))?;
                let adapter = Arc::new(adapter);
                let stream_adapter = adapter.clone();
                Ok(Connected {
                    daily_stats: Some(adapter.clone()),
                    adapter,
                    exec_stream: Some(Arc::new(move |health, engine| {
                        Box::pin(crate::connectors::binance::stream::run(env, stream_adapter.clone(), health, engine))
                    })),
                })
            }
        }
    }

    fn public_feed(&self) -> Option<PublicFeed> {
        Some(PublicFeed {
            symbology: &crate::connectors::binance::feed::BinanceFeed,
            label: "BINANCE/SPOT",
            source: || Arc::new(crate::connectors::binance::feed::BinanceFeed),
        })
    }

    /// Not tested on either transport (as before connectors).
    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { FIX_NOT_TESTABLE })
    }

    fn universe_provider(&self, env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapter::InstrumentProvider>, String> {
        let c: crate::connectors::binance::adapter::BinanceCredentials = typed(creds, MANIFEST.credential_kind)?;
        Ok(Box::new(crate::connectors::binance::adapter::BinanceAdapter::new(c.api_key, &c.private_key, env)?))
    }
}

fn venue() -> Venue {
    Venue {
        code: "BINANCE",
        name: "Binance",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/binance/"),
        universe: Some("About 2,000 spot pairs"),
        environments: environments(
            "spot testnet",
            Some("https://testnet.binance.vision/"),
            Some("https://www.binance.com/en/my/settings/api-management"),
        ),
        fields: vec![
            field("api_key", "API key").secret(),
            field("private_key", "Private key (PEM)").secret().input(Input::Textarea),
            field("host", "Host").fix().prefill("fix-oe.testnet.binance.vision", "fix-oe.binance.com"),
            field("port", "Port").fix().prefill("9000", "9000"),
            field("sender_comp_id", "SenderCompID").fix().prefill("OMS", "OMS"),
            field("target_comp_id", "TargetCompID").fix().prefill("SPOT", "SPOT"),
        ],
    }
}

/// Wire a Binance Spot FIX order-entry session for `env_name` from an
/// already-decrypted credential. `creds` must be a `BinanceFix` credential
/// — see `start_ibkr`'s doc comment for why a mismatch is logged, not panicked.
/// `private_key` is PEM contents (not a path) — the store holds the key material
/// itself, so there is nothing to read from disk here.
fn start_binance(
    env_name: &str,
    creds: &crate::credentials::BrokerCredentials,
    stream_health: &StreamHealthRegistry,
    engine: EngineHandle,
) -> Option<Arc<FixBrokerAdapter>> {
    let parsed = match creds.kind.as_str() {
        crate::connectors::binance::adapter::BinanceCredentials::KIND => {
            creds.parse::<crate::connectors::binance::adapter::BinanceCredentials>().ok()
        }
        _ => None,
    };
    let Some(parsed) = parsed else {
        error!(env = env_name, "start_binance called with non-Binance credentials (programming error)");
        return None;
    };
    let crate::connectors::binance::adapter::BinanceCredentials {
        host, port, sender_comp_id, target_comp_id, api_key, private_key,
    } = &parsed;
    // A REST-only Binance credential stores an empty host (see `import_env::scan_env`
    // and `save_broker` from the cockpit-to-be — REST never reads a FIX host, so
    // nothing requires one to be set). Asking for FIX transport on such a
    // credential must fail clearly here, before ever touching the network, rather
    // than attempting a QuickFIX connection to `SocketConnectHost = ""`.
    if host.is_empty() {
        error!(
            env = env_name,
            "start_binance: credential has no FIX host configured — set it (e.g. via \
             BINANCE_{{ENV}}_FIX_HOST before `oms config import-env`, or the cockpit \
             once it can edit credentials) before selecting FIX transport"
        );
        return None;
    }
    let dialect = match BinanceDialect::new(api_key.clone(), private_key) {
        Ok(d) => Arc::new(d) as Arc<dyn FixDialect>,
        Err(e) => { error!(env = env_name, error = %e, "Binance FIX dialect init failed"); return None; }
    };
    // Binance Spot FIX has no open-orders / order-status message, so route the
    // order-reconciliation reads through the same credential's REST API. Orders still
    // go over FIX; only the recon snapshot/status reads use REST.
    let read_delegate: Option<Arc<dyn crate::adapter::BrokerAdapter>> =
        match crate::connectors::binance::adapter::BinanceAdapter::new(api_key.clone(), private_key, env_name) {
            Ok(a) => Some(Arc::new(a)),
            Err(e) => {
                error!(env = env_name, error = %e, "Binance FIX: REST recon delegate init failed; recon disabled");
                None
            }
        };
    let cfg = FixConfig {
        host: host.clone(),
        port: *port,
        sender_comp_id: sender_comp_id.clone(),
        target_comp_id: target_comp_id.clone(),
        ssl: true, // Binance FIX requires TLS.
        heartbeat_secs: 30,
    };
    // Seed the health entry only now that we know the session is configured.
    let health = stream_health.fix_handle("BINANCE", env_name);
    match start_session(dialect, cfg, "binance", health, engine, read_delegate) {
        Ok(a) => { info!(env = env_name, "registered BINANCE FIX adapter"); Some(a) }
        Err(e) => { error!(env = env_name, error = %e, "Binance FIX session not started"); None }
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

    fn ibkr_cred() -> BrokerCredentials {
        BrokerCredentials::from_typed("IbkrFix", &crate::connectors::ibkr::fix::IbkrFixCredentials {
            host: "h".into(),
            port: 4001,
            sender_comp_id: "S".into(),
            target_comp_id: "T".into(),
            password: "p".into(),
            ssl: true,
        })
    }


    /// Same guard, the other direction.
    #[tokio::test]
    async fn start_binance_refuses_a_non_binance_credential() {
        let health = StreamHealthRegistry::new();
        let result = start_binance("PAPER", &ibkr_cred(), &health, idle_engine());
        assert!(result.is_none());
    }

    /// The empty-host guard added this round: a REST-only Binance credential
    /// (empty `host`, per `import_env::scan_env` — REST never reads a FIX host)
    /// must refuse FIX transport rather than attempt a QuickFIX connection to
    /// `SocketConnectHost = ""`.
    #[tokio::test]
    async fn start_binance_refuses_an_empty_fix_host() {
        let health = StreamHealthRegistry::new();
        let creds = BrokerCredentials::from_typed("BinanceFix", &crate::connectors::binance::adapter::BinanceCredentials {
            host: "".into(),
            port: 9000,
            sender_comp_id: "OMS".into(),
            target_comp_id: "SPOT".into(),
            api_key: "k".into(),
            private_key: "pem".into(),
        });
        let result = start_binance("PAPER", &creds, &health, idle_engine());
        assert!(result.is_none());
    }
}
