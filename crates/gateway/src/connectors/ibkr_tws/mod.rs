pub mod exchanges;

use futures_util::future::BoxFuture;

use crate::connectors::RegistrationDeps;
use crate::credentials::BrokerCredentials;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Venue, DOCS};
use crate::verify::TestOutcome;

use super::{Connected, Connector, Manifest, QuoteScope};

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

    fn connect(&self, _: &'static str, _: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        Err("not yet wired".into())
    }

    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { TestOutcome::NotTestable("checked when the gateway logs in") })
    }
}

fn venue() -> Venue {
    Venue {
        code: "IBKR_TWS",
        name: "Interactive Brokers (TWS API)",
        description: "Stocks worldwide via IB Gateway (Docker)",
        docs_url: format!("{DOCS}/interactive-brokers-tws/"),
        universe: Some("Imported on demand from a CSV or search"),
        environments: environments(
            "paper trading account",
            Some("https://www.interactivebrokers.com/portal/"),
            Some("https://www.interactivebrokers.com/portal/"),
        ),
        fields: vec![field("username", "IBKR username"), field("password", "Password").secret()],
    }
}
