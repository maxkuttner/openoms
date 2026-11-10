pub mod adapter;
pub mod client;
pub mod exchanges;
pub mod gateway;
pub mod resolve;
pub mod stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;

use crate::connectors::RegistrationDeps;
use crate::credentials::BrokerCredentials;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Venue, DOCS};
use crate::verify::TestOutcome;

use super::{typed, Connected, Connector, Manifest, QuoteScope};

pub struct IbkrTws;

/// Connected clients by connection code (`IBKR_TWS-PAPER`), set by the
/// supervisor while a session is up. How the admin API (contract search and
/// import) reaches IB Gateway without owning the connection.
static LIVE: Mutex<Option<HashMap<String, Arc<dyn client::TwsClient>>>> = Mutex::new(None);

/// The connection code the supervisor registers its client under.
pub fn connection_code(env: &str) -> String {
    format!("{}-{env}", MANIFEST.code)
}

/// The connected client for `connection_code`, if its session is up.
pub fn live_client(connection_code: &str) -> Option<Arc<dyn client::TwsClient>> {
    LIVE.lock().unwrap().as_ref()?.get(connection_code).filter(|c| c.connected()).cloned()
}

pub(crate) fn set_live_client(connection_code: &str, client: Option<Arc<dyn client::TwsClient>>) {
    let mut live = LIVE.lock().unwrap();
    let map = live.get_or_insert_with(HashMap::new);
    match client {
        Some(c) => map.insert(connection_code.to_string(), c),
        None => map.remove(connection_code),
    };
}

/// An IBKR login for the IB Gateway container.
/// Stored as `{"kind":"IbkrTws", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IbkrTwsCredentials {
    pub username: String,
    pub password: String,
}

static MANIFEST: Manifest = Manifest {
    code: "IBKR_TWS",
    credential_kind: "IbkrTws",
    venue,
    field_types: &[],
    redaction: &[("username", Clear), ("password", Hidden)],
    live_only: false,
    universe: false,
    quotes: QuoteScope::NonCrypto,
    exec_stream: true,
    session_bound: false,
    price_band_exempt: true,
    per_currency_notional: true,
};

impl Connector for IbkrTws {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// Builds the adapter now; the execution stream owns the gateway: it starts
    /// or adopts the container, connects, and hands the client to the adapter.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let creds: IbkrTwsCredentials = typed(creds, MANIFEST.credential_kind)?;
        let state_dir = state_dir().ok_or("no HOME or XDG_STATE_HOME to keep the IB Gateway env file under")?;
        let slot: adapter::ClientSlot = Arc::new(RwLock::new(None));
        let adapter = Arc::new(adapter::IbkrTwsAdapter::new(slot.clone()));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            daily_stats: Some(adapter.clone()),
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                let gw = stream::Gateway {
                    docker: Arc::new(gateway::DockerCli),
                    code: format!("{}-{env}", MANIFEST.code),
                    env,
                    creds: creds.clone(),
                    state_dir: state_dir.clone(),
                };
                let ctx = stream::Ctx { adapter: stream_adapter.clone(), health, engine, env };
                Box::pin(stream::supervise(gw, slot.clone(), ctx))
            })),
        })
    }

    fn universe_provider(&self, _: &str, _: &BrokerCredentials) -> Result<Box<dyn crate::adapter::InstrumentProvider>, String> {
        Err("IBKR instruments are imported from a CSV or search".into())
    }

    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { TestOutcome::NotTestable("checked when the gateway logs in") })
    }

    /// Stops and removes the connection's IB Gateway container.
    fn on_removed(&self, env: &str) -> BoxFuture<'static, ()> {
        Box::pin(gateway::remove(Arc::new(gateway::DockerCli), connection_code(env)))
    }
}

/// `$XDG_STATE_HOME/oms/ibkr`, else `~/.local/state/oms/ibkr`: where the
/// short-lived gateway env file is written (mode 0600, deleted after `docker run`).
fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("oms").join("ibkr"))
}

fn venue() -> Venue {
    Venue {
        code: "IBKR_TWS",
        name: "Interactive Brokers (TWS API)",
        description: "Stocks worldwide via IB Gateway (Docker)",
        docs_url: format!("{DOCS}/interactive-brokers/"),
        universe: Some("Imported on demand from a CSV or search"),
        environments: environments(
            "paper trading account",
            Some("https://www.interactivebrokers.com/portal/"),
            Some("https://www.interactivebrokers.com/portal/"),
        ),
        fields: vec![field("username", "IBKR username"), field("password", "Password").secret()],
    }
}
