use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{typed, Connected};
use crate::credentials::Shown::{Hidden, Tail};
use crate::venues::{environments, field, Venue, DOCS};

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
};

impl Connector for Alpaca {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::adapters::alpaca::AlpacaCredentials = typed(creds)?;
        let adapter = Arc::new(crate::adapters::alpaca::AlpacaAdapter::new(c.key.clone(), c.secret.clone(), env));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::alpaca_stream::run(env, c.key.clone(), c.secret.clone(), stream_adapter.clone(), health, engine))
            })),
        })
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
