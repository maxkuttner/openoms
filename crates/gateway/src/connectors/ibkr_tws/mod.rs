pub mod adapter;
pub mod client;
pub mod exchanges;
pub mod gateway;
pub mod stream;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use futures_util::future::BoxFuture;

use crate::connectors::RegistrationDeps;
use crate::credentials::BrokerCredentials;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Venue, DOCS};
use crate::verify::TestOutcome;

use super::{typed, Connected, Connector, Manifest, QuoteScope};

pub struct IbkrTws;

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
    universe: true,
    quotes: QuoteScope::NonCrypto,
    exec_stream: true,
    session_bound: true,
    price_band_exempt: true,
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

    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { TestOutcome::NotTestable("checked when the gateway logs in") })
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
