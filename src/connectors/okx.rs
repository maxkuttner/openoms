use futures_util::future::BoxFuture;

use crate::credentials_api::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{checked, typed, Connected, PublicFeed};
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

    fn public_feed(&self) -> Option<PublicFeed> {
        Some(PublicFeed {
            symbology: &crate::okx_feed::OkxFeed,
            run: |pool, quotes, order_rx, health| {
                Box::pin(async move {
                    let session = crate::quote_feed::QuoteFeedSession::new(
                        crate::okx_feed::OkxFeed,
                        pool,
                        quotes,
                        order_rx,
                        health.clone(),
                    );
                    crate::stream_supervisor::supervise("OKX/SPOT", health, session).await;
                })
            },
        })
    }

    fn verify<'a>(&'a self, env: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async move {
            let c: crate::adapters::okx::OkxCredentials = match typed(creds) {
                Ok(c) => c,
                Err(e) => return TestOutcome::Failed(e),
            };
            let adapter = crate::adapters::okx::OkxAdapter::new(c.api_key, c.api_secret, c.passphrase, env);
            checked("OKX", adapter.key_permissions(), |perm| judge_okx_permissions(&perm)).await
        })
    }

    fn universe_provider(&self, env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapters::InstrumentProvider>, String> {
        let c: crate::adapters::okx::OkxCredentials = typed(creds)?;
        Ok(Box::new(crate::adapters::okx::OkxAdapter::new(c.api_key, c.api_secret, c.passphrase, env)))
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

/// An OKX key is fit for the OMS when it can trade and cannot withdraw — the
/// same rule as Coinbase. `perm` is a comma list such as "read_only,trade".
fn judge_okx_permissions(perm: &str) -> TestOutcome {
    let has = |p: &str| perm.split(',').any(|x| x.trim() == p);
    if has("withdraw") {
        TestOutcome::Failed("the key can withdraw funds; create one with Read and Trade permissions only".into())
    } else if !has("trade") {
        TestOutcome::Failed("the key cannot trade; give it the Trade permission".into())
    } else {
        TestOutcome::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_okx_key_must_trade_and_must_not_withdraw() {
        assert_eq!(judge_okx_permissions("read_only,trade"), TestOutcome::Passed);
        assert!(matches!(judge_okx_permissions("read_only,withdraw,trade"), TestOutcome::Failed(m) if m.contains("withdraw")));
        assert!(matches!(judge_okx_permissions("read_only"), TestOutcome::Failed(m) if m.contains("trade")));
    }
}
