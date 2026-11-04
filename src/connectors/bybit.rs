use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{typed, Connected};
use crate::credentials::Shown::Hidden;
use crate::venues::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Bybit;

static MANIFEST: Manifest = Manifest {
    code: "BYBIT",
    credential_kind: "Bybit",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("api_secret", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
    exec_stream: true,
    session_bound: false,
};

impl Connector for Bybit {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::adapters::bybit::BybitCredentials = typed(creds)?;
        let adapter = Arc::new(crate::adapters::bybit::BybitAdapter::new(c.api_key, c.api_secret, env));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::bybit_stream::run(env, stream_adapter.clone(), health, engine))
            })),
        })
    }
}

fn venue() -> Venue {
    Venue {
        code: "BYBIT",
        name: "Bybit",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/bybit/"),
        universe: Some("About 800 spot pairs"),
        environments: environments(
            "spot testnet",
            Some("https://testnet.bybit.com/app/user/api-management"),
            Some("https://www.bybit.com/app/user/api-management"),
        ),
        fields: vec![field("api_key", "API key").secret(), field("api_secret", "API secret").secret()],
    }
}
