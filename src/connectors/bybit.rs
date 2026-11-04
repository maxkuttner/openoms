use futures_util::future::BoxFuture;

use crate::credentials_api::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{checked, typed, Connected, PublicFeed};
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
        let c: crate::adapters::bybit::BybitCredentials = typed(creds, MANIFEST.credential_kind)?;
        let adapter = Arc::new(crate::adapters::bybit::BybitAdapter::new(c.api_key, c.api_secret, env));
        let stream_adapter = adapter.clone();
        Ok(Connected {
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::bybit_stream::run(env, stream_adapter.clone(), health, engine))
            })),
        })
    }

    fn public_feed(&self) -> Option<PublicFeed> {
        Some(PublicFeed {
            symbology: &crate::bybit_feed::BybitFeed,
            run: |pool, quotes, order_rx, health| {
                Box::pin(async move {
                    let session = crate::quote_feed::QuoteFeedSession::new(
                        crate::bybit_feed::BybitFeed,
                        pool,
                        quotes,
                        order_rx,
                        health.clone(),
                    );
                    gateway::stream_supervisor::supervise("BYBIT/SPOT", health, session).await;
                })
            },
        })
    }

    fn verify<'a>(&'a self, env: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async move {
            let c: crate::adapters::bybit::BybitCredentials = match typed(creds, MANIFEST.credential_kind) {
                Ok(c) => c,
                Err(e) => return TestOutcome::Failed(e),
            };
            let adapter = crate::adapters::bybit::BybitAdapter::new(c.api_key, c.api_secret, env);
            checked("Bybit", adapter.api_key_info(), |_| TestOutcome::Passed).await
        })
    }

    fn universe_provider(&self, env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapters::InstrumentProvider>, String> {
        let c: crate::adapters::bybit::BybitCredentials = typed(creds, MANIFEST.credential_kind)?;
        Ok(Box::new(crate::adapters::bybit::BybitAdapter::new(c.api_key, c.api_secret, env)))
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
