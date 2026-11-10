pub mod adapter;
pub mod stream;
pub mod feed;

use futures_util::future::BoxFuture;

use crate::verify::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::connectors::RegistrationDeps;

use super::{checked, typed, Connected, PublicFeed};
use crate::credentials::Shown::Hidden;
use crate::venue::{field, Environment, Input, Venue, DOCS};

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
    price_band_exempt: false,
    per_currency_notional: false,
};

impl Connector for Coinbase {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let c: crate::connectors::coinbase::adapter::CoinbaseCredentials = typed(creds, MANIFEST.credential_kind)?;
        let adapter = crate::connectors::coinbase::adapter::CoinbaseAdapter::new(c.api_key, &c.private_key)
            .map_err(|e| format!("Coinbase adapter not registered: {e}"))?;
        let adapter = Arc::new(adapter);
        let stream_adapter = adapter.clone();
        Ok(Connected {
            daily_stats: Some(adapter.clone()),
            adapter,
            exec_stream: Some(Arc::new(move |health, engine| {
                Box::pin(crate::connectors::coinbase::stream::run(env, stream_adapter.clone(), health, engine))
            })),
        })
    }

    fn public_feed(&self) -> Option<PublicFeed> {
        Some(PublicFeed {
            symbology: &crate::connectors::coinbase::feed::CoinbaseFeed,
            label: "COINBASE/SPOT",
            source: || Arc::new(crate::connectors::coinbase::feed::CoinbaseFeed),
        })
    }

    fn verify<'a>(&'a self, _: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async move {
            let c: crate::connectors::coinbase::adapter::CoinbaseCredentials = match typed(creds, MANIFEST.credential_kind) {
                Ok(c) => c,
                Err(e) => return TestOutcome::Failed(e),
            };
            let adapter = match crate::connectors::coinbase::adapter::CoinbaseAdapter::new(c.api_key, &c.private_key) {
                Ok(a) => a,
                Err(e) => return TestOutcome::Failed(e),
            };
            checked("Coinbase", adapter.key_permissions(), |p| judge_coinbase_permissions(&p)).await
        })
    }

    fn universe_provider(&self, _env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapter::InstrumentProvider>, String> {
        let c: crate::connectors::coinbase::adapter::CoinbaseCredentials = typed(creds, MANIFEST.credential_kind)?;
        Ok(Box::new(crate::connectors::coinbase::adapter::CoinbaseAdapter::new(c.api_key, &c.private_key)?))
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

/// A Coinbase key is fit for the OMS when it can trade and cannot move money
/// out: an OMS never needs to withdraw, so a transfer-capable key is refused.
fn judge_coinbase_permissions(permissions: &serde_json::Value) -> TestOutcome {
    if permissions["can_transfer"].as_bool() != Some(false) {
        TestOutcome::Failed("the key can transfer funds; create one with View and Trade permissions only".into())
    } else if permissions["can_trade"].as_bool() != Some(true) {
        TestOutcome::Failed("the key cannot trade; give it the Trade permission".into())
    } else {
        TestOutcome::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_can_transfer_fails_the_test() {
        let judge = |v: serde_json::Value| judge_coinbase_permissions(&v);
        assert_eq!(judge(serde_json::json!({"can_view": true, "can_trade": true, "can_transfer": false})), TestOutcome::Passed);
        assert!(matches!(
            judge(serde_json::json!({"can_view": true, "can_trade": true, "can_transfer": true})),
            TestOutcome::Failed(m) if m.contains("transfer")
        ));
        assert!(matches!(
            judge(serde_json::json!({"can_view": true, "can_trade": false, "can_transfer": false})),
            TestOutcome::Failed(m) if m.contains("trade")
        ));
    }
}
