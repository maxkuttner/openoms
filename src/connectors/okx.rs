use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{typed, Connected};
use crate::credentials::Shown::Hidden;
use crate::venues::{environments, field, Venue, DOCS};

use super::{Connector, Manifest, QuoteScope};

pub struct Okx;

static MANIFEST: Manifest = Manifest {
    code: "OKX",
    credential_kind: "Okx",
    venue,
    field_types: &[],
    redaction: &[("api_key", Hidden), ("api_secret", Hidden), ("passphrase", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
    exec_stream: true,
    session_bound: false,
};

impl Connector for Okx {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::adapters::okx::OkxCredentials = typed(creds)?;
        let adapter = Arc::new(crate::adapters::okx::OkxAdapter::new(c.api_key, c.api_secret, c.passphrase, env));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::okx_stream::run(env, stream_adapter.clone(), health, engine))
            })),
        })
    }
}

fn venue() -> Venue {
    Venue {
        code: "OKX",
        name: "OKX",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/okx/"),
        universe: Some("About 700 spot pairs"),
        // Demo Trading keys are made in the same account, under Demo Trading.
        environments: environments(
            "demo trading",
            Some("https://www.okx.com/account/my-api"),
            Some("https://www.okx.com/account/my-api"),
        ),
        fields: vec![
            field("api_key", "API key").secret(),
            field("api_secret", "API secret").secret(),
            field("passphrase", "Passphrase").secret(),
        ],
    }
}
