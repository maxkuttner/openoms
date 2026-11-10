pub mod adapter;
pub mod stream;

use futures_util::future::BoxFuture;

use crate::verify::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::connectors::RegistrationDeps;

use super::{checked, typed, Connected};
use crate::credentials::Shown::{Hidden, Tail};
use crate::venue::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Alpaca;

static MANIFEST: Manifest = Manifest {
    code: "ALPACA",
    credential_kind: "Alpaca",
    venue,
    field_types: &[],
    redaction: &[("key", Tail), ("secret", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::NonCrypto,
    exec_stream: true,
    session_bound: false,
    price_band_exempt: false,
};

impl Connector for Alpaca {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::connectors::alpaca::adapter::AlpacaCredentials = typed(creds, MANIFEST.credential_kind)?;
        let adapter = Arc::new(crate::connectors::alpaca::adapter::AlpacaAdapter::new(c.key.clone(), c.secret.clone(), env));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            daily_stats: Some(adapter.clone()),
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::connectors::alpaca::stream::run(env, c.key.clone(), c.secret.clone(), stream_adapter.clone(), health, engine))
            })),
        })
    }

    /// `GET /v2/account`: lightweight, read-only, sub-second. `env` must be the
    /// connection's own — the adapter picks the live endpoint only for exactly
    /// "LIVE", so a live key tested as PAPER would be refused though valid.
    fn verify<'a>(&'a self, env: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async move {
            let c: crate::connectors::alpaca::adapter::AlpacaCredentials = match typed(creds, MANIFEST.credential_kind) {
                Ok(c) => c,
                Err(e) => return TestOutcome::Failed(e),
            };
            let adapter = crate::connectors::alpaca::adapter::AlpacaAdapter::new(c.key, c.secret, env);
            checked("Alpaca", adapter.get_account(), |_| TestOutcome::Passed).await
        })
    }

    fn universe_provider(&self, env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapter::InstrumentProvider>, String> {
        let c: crate::connectors::alpaca::adapter::AlpacaCredentials = typed(creds, MANIFEST.credential_kind)?;
        Ok(Box::new(crate::connectors::alpaca::adapter::AlpacaAdapter::new(c.key, c.secret, env)))
    }
}

fn venue() -> Venue {
    Venue {
        code: "ALPACA",
        name: "Alpaca",
        description: "US stocks, ETFs and options",
        docs_url: format!("{DOCS}/alpaca/"),
        universe: Some("About 14,400 stocks and ETFs"),
        environments: environments(
            "paper trading account",
            Some("https://app.alpaca.markets/paper/dashboard/overview"),
            Some("https://app.alpaca.markets/brokerage/dashboard/overview"),
        ),
        fields: vec![field("key", "API key ID"), field("secret", "Secret key").secret()],
    }
}
