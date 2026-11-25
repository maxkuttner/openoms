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
use serde::Deserialize;

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
    /// The TWS API client id the OMS connects as (default 0). Client 0 with
    /// `reqAutoOpenOrders(true)` also sees orders typed into TWS, so they are
    /// adopted. Orders and executions of *other API clients* reach only the
    /// client whose id equals IB Gateway's "Master API client ID" setting
    /// (Configure > API > Settings), which the OMS does not set: set it there
    /// by hand to this value to see them.
    #[serde(default, deserialize_with = "client_id")]
    pub master_client_id: i32,
}

/// A client id stored as a number or as the form's text (empty = 0).
fn client_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    use serde::de::Error;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or_else(|| D::Error::custom("master_client_id out of range")),
        serde_json::Value::String(s) if s.trim().is_empty() => Ok(0),
        serde_json::Value::String(s) => s.trim().parse().map_err(|_| D::Error::custom(format!("master_client_id is not a number: {s}"))),
        serde_json::Value::Null => Ok(0),
        other => Err(D::Error::custom(format!("master_client_id is not a number: {other}"))),
    }
}

static MANIFEST: Manifest = Manifest {
    code: "IBKR_TWS",
    credential_kind: "IbkrTws",
    venue,
    field_types: &[],
    redaction: &[("username", Clear), ("password", Hidden), ("master_client_id", Clear)],
    live_only: false,
    universe: false,
    quotes: QuoteScope::NonCrypto,
    exec_stream: true,
    session_bound: false,
    price_band_exempt: true,
    per_currency_notional: true,
    multi_account: true,
    instrument_classes: &["SPOT","FUTURE","OPTION"],
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
        // Venue events only: the reconciler books every fill (exactly once,
        // through ingest) and pulls what the stream missed.
        Ok(Connected {
            source: Some(adapter.clone()),
            daily_stats: Some(adapter.clone()),
            adapter,
            venue_stream: Some(Arc::new(move |health, tx| {
                let gw = stream::Gateway {
                    docker: Arc::new(gateway::DockerCli),
                    code: format!("{}-{env}", MANIFEST.code),
                    env,
                    creds: creds.clone(),
                    state_dir: state_dir.clone(),
                };
                let ctx = stream::Ctx { adapter: stream_adapter.clone(), health, tx };
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
        fields: vec![
            field("username", "IBKR username"),
            field("password", "Password").secret(),
            field("master_client_id", "API client id (0 also sees orders entered in TWS; to see other API clients' orders set IB Gateway's Master API client ID to this value)").prefill("0", "0"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_comes_from_credentials() {
        let parse = |v: serde_json::Value| serde_json::from_value::<IbkrTwsCredentials>(v).map(|c| c.master_client_id);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p"})).unwrap(), 0, "default: master");
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": 7})).unwrap(), 7);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": "12"})).unwrap(), 12);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": ""})).unwrap(), 0);
        assert!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": "x"})).is_err());
    }
}
