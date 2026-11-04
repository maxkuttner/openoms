use futures_util::future::BoxFuture;

use crate::credentials_api::TestOutcome;
use std::sync::Arc;

use crate::credentials::BrokerCredentials;
use crate::reload::RegistrationDeps;

use super::{FIX_NOT_TESTABLE, PublicFeed, typed, Connected};
use crate::credentials::Shown::{Clear, Hidden};
use crate::venues::{environments, field, Input, Venue, DOCS};

use super::{Connector, FieldType, Manifest, QuoteScope};

pub struct Binance;

static MANIFEST: Manifest = Manifest {
    code: "BINANCE",
    credential_kind: "BinanceFix",
    venue,
    field_types: &[("port", FieldType::Port)],
    redaction: &[("host", Clear), ("port", Clear), ("sender_comp_id", Clear), ("target_comp_id", Clear), ("api_key", Hidden), ("private_key", Hidden)],
    live_only: false,
    universe: true,
    quotes: QuoteScope::OwnVenue,
    exec_stream: true,
    session_bound: true,
};

impl Connector for Binance {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// Transport is an explicit choice via `BINANCE_{ENV}_TRANSPORT=fix|rest`
    /// (default rest) — a wire-protocol setting, not a secret, so it stays on the
    /// environment. `fix` runs one FIX session for order entry and execution
    /// reports; `rest` runs the REST adapter plus the WS user-data stream, built
    /// from the same stored credential.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String> {
        use crate::adapters::Transport;
        match Transport::from_env(&format!("BINANCE_{env}"), Transport::Rest) {
            Transport::Fix => {
                let adapter = crate::fix::start_binance(env, creds, &deps.stream_health, deps.engine.clone())
                    .ok_or_else(|| "Binance FIX session could not start".to_string())?;
                Ok(Connected { adapter, exec_stream: None })
            }
            Transport::Rest => {
                let c: crate::adapters::binance::BinanceCredentials = typed(creds)?;
                let adapter = crate::adapters::binance::BinanceAdapter::new(c.api_key, &c.private_key, env)
                    .map_err(|e| format!("Binance adapter not registered: {e}"))?;
                let adapter = Arc::new(adapter);
                let stream_adapter = adapter.clone();
                Ok(Connected {
                    adapter,
                    exec_stream: Some(Arc::new(move |health, engine| {
                        Box::pin(crate::binance_stream::run(env, stream_adapter.clone(), health, engine))
                    })),
                })
            }
        }
    }

    fn public_feed(&self) -> Option<PublicFeed> {
        Some(PublicFeed {
            symbology: &crate::binance_feed::BinanceFeed,
            run: |pool, quotes, order_rx, health| {
                Box::pin(async move {
                    let session = crate::quote_feed::QuoteFeedSession::new(
                        crate::binance_feed::BinanceFeed,
                        pool,
                        quotes,
                        order_rx,
                        health.clone(),
                    );
                    crate::stream_supervisor::supervise("BINANCE/SPOT", health, session).await;
                })
            },
        })
    }

    /// Not tested on either transport (as before connectors).
    fn verify<'a>(&'a self, _: &'a str, _: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async { FIX_NOT_TESTABLE })
    }

    fn universe_provider(&self, env: &str, creds: &BrokerCredentials) -> Result<Box<dyn crate::adapters::InstrumentProvider>, String> {
        let c: crate::adapters::binance::BinanceCredentials = typed(creds)?;
        Ok(Box::new(crate::adapters::binance::BinanceAdapter::new(c.api_key, &c.private_key, env)?))
    }
}

fn venue() -> Venue {
    Venue {
        code: "BINANCE",
        name: "Binance",
        description: "Crypto spot pairs",
        docs_url: format!("{DOCS}/binance/"),
        universe: Some("About 2,000 spot pairs"),
        environments: environments(
            "spot testnet",
            Some("https://testnet.binance.vision/"),
            Some("https://www.binance.com/en/my/settings/api-management"),
        ),
        fields: vec![
            field("api_key", "API key").secret(),
            field("private_key", "Private key (PEM)").secret().input(Input::Textarea),
            field("host", "Host").fix().prefill("fix-oe.testnet.binance.vision", "fix-oe.binance.com"),
            field("port", "Port").fix().prefill("9000", "9000"),
            field("sender_comp_id", "SenderCompID").fix().prefill("OMS", "OMS"),
            field("target_comp_id", "TargetCompID").fix().prefill("SPOT", "SPOT"),
        ],
    }
}
