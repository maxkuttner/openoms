use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{typed, Connected};
use crate::credentials::Shown::Hidden;
use crate::venues::{field, Environment, Input, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Coinbase;

static MANIFEST: Manifest = Manifest {
    code: "COINBASE",
    credential_kind: "Coinbase",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("private_key", Hidden)],
    live_only: true,
    universe: true,
    quotes: QuoteScope::OwnVenue,
    exec_stream: true,
    session_bound: false,
};

impl Connector for Coinbase {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::adapters::coinbase::CoinbaseCredentials = typed(creds)?;
        let adapter = crate::adapters::coinbase::CoinbaseAdapter::new(c.api_key, &c.private_key)
            .map_err(|e| format!("Coinbase adapter not registered: {e}"))?;
        let adapter = Arc::new(adapter);
        let stream_adapter = adapter.clone();
        Ok(Connected {
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::coinbase_stream::run(env, stream_adapter.clone(), health, engine))
            })),
        })
    }
}

fn venue() -> Venue {
    Venue {
        code: "COINBASE",
        name: "Coinbase",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/coinbase/"),
        universe: Some("About 900 spot pairs"),
        // LIVE only: Coinbase's sandbox returns canned responses, not paper trading.
        environments: vec![Environment {
            code: "LIVE",
            label: "live account",
            keys_url: Some("https://portal.cdp.coinbase.com/projects/api-keys"),
        }],
        fields: vec![
            field("api_key", "API key").secret(),
            field("private_key", "Private key (PEM)").secret().input(Input::Textarea),
        ],
    }
}
