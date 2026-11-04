//! Connectors: one module per broker, each describing itself with a [`Manifest`].
//!
//! The central parts of the OMS — the connection form, credential handling,
//! registration, stream restarts — read what they need from [`all()`] instead of
//! naming brokers. Adding one is its module here, its line in `all()`, its
//! migration and its docs page.

use std::sync::Arc;

use dataprovider::{FeedSymbology, LiveQuoteFeed};
use futures_util::future::BoxFuture;

use crate::adapter::{BrokerAdapter, InstrumentProvider};
use crate::credentials::{BrokerCredentials, Shown};
use crate::verify::TestOutcome;
use crate::contract::EngineHandle;
use crate::stream_health::StreamHealthRegistry;
use crate::stream_health::StreamHandle;
use crate::venue::Venue;

pub mod alpaca;
pub mod binance;
pub mod bybit;
pub mod coinbase;
pub mod ibkr;
pub mod okx;

/// How a non-text credential field is typed in the stored JSON.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FieldType {
    Port,
    Bool { default: bool },
}

/// Which instruments' daily stats this broker can be asked for.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum QuoteScope {
    /// Not wired for daily stats.
    None,
    /// Its own venue's instruments only (a crypto exchange).
    OwnVenue,
    /// Every venue no other connector quotes as its own (Alpaca: the equity MICs).
    NonCrypto,
}

pub struct Manifest {
    /// `broker_connection.broker_code`.
    pub code: &'static str,
    /// The stored credential's `kind` tag.
    pub credential_kind: &'static str,
    /// Its entry in the cockpit's venue list.
    pub venue: fn() -> Venue,
    /// Fields that are not plain text (port numbers, flags); everything else is text.
    pub field_types: &'static [(&'static str, FieldType)],
    /// How each stored field may be shown, in display order.
    pub redaction: &'static [(&'static str, Shown)],
    /// No paper environment: any non-LIVE connection would trade real money.
    pub live_only: bool,
    /// Has an instrument universe to load.
    pub universe: bool,
    pub quotes: QuoteScope,
    /// Runs an execution stream apart from its adapter (REST connectors), which
    /// is stopped when the connection goes away.
    pub exec_stream: bool,
    /// Its session owns a running logon (FIX): a credential change needs a
    /// restart, and on reload the running adapter is carried forward instead.
    pub session_bound: bool,
}

/// What the FIX starters and freshly-built adapters need beyond the credential
/// itself. Built once in `serve()` and handed to every call of `build_registry`
/// — boot today, a runtime reload endpoint in a later task — so both draw on the
/// exact same stream-health registry and order engine rather than each
/// constructing (or re-deriving) their own.
pub struct RegistrationDeps {
    pub stream_health: StreamHealthRegistry,
    pub engine: EngineHandle,
}

/// Starts one execution stream; called again on every restart.
pub type ExecStream = Arc<dyn Fn(StreamHandle, EngineHandle) -> BoxFuture<'static, ()> + Send + Sync>;

/// A live quote source the OMS drives: it knows what it can price and how to stream it.
pub trait QuoteSource: LiveQuoteFeed + FeedSymbology {}
impl<T: LiveQuoteFeed + FeedSymbology> QuoteSource for T {}

/// A venue's public quote feed: needs no credential, runs for the life of the
/// process, and prices the instruments it covers that are held. The OMS drives
/// it, since what is held lives in the database.
pub struct PublicFeed {
    pub symbology: &'static dyn FeedSymbology,
    /// Its supervisor's name in the stream registry, e.g. "OKX/SPOT".
    pub label: &'static str,
    pub source: fn() -> Arc<dyn QuoteSource>,
}

/// What connecting one configured connection produced.
pub struct Connected {
    pub adapter: Arc<dyn BrokerAdapter>,
    /// The execution stream to (re)start for it; `None` when the session that
    /// routes orders also delivers fills (FIX).
    pub exec_stream: Option<ExecStream>,
}

pub trait Connector: Send + Sync {
    fn manifest(&self) -> &'static Manifest;

    /// Build the adapter (and start a FIX session) for one connection. `Err` is
    /// the reason the connection is reported failed.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String>;

    /// Check a credential against the venue before it is saved, where that is
    /// cheap and side-effect free.
    fn verify<'a>(&'a self, env: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome>;

    /// The data provider that loads this broker's instrument universe, built
    /// from its stored credential. Only connectors whose manifest has `universe`.
    fn universe_provider(&self, _env: &str, _creds: &BrokerCredentials) -> Result<Box<dyn InstrumentProvider>, String> {
        Err("no instrument universe to load".into())
    }

    /// Its public quote feed, if the venue has one.
    fn public_feed(&self) -> Option<PublicFeed> {
        None
    }
}

/// Run a verification call under the credential-test timeout, mapping its
/// outcome; `provider` names it in the timeout message.
async fn checked<T, E: std::fmt::Display>(
    provider: &str,
    call: impl std::future::Future<Output = Result<T, E>>,
    judge: impl FnOnce(T) -> TestOutcome,
) -> TestOutcome {
    match tokio::time::timeout(crate::verify::TEST_TIMEOUT, call).await {
        Ok(Ok(v)) => judge(v),
        Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
        Err(_) => crate::verify::timed_out(provider),
    }
}

/// A FIX credential is checked by logging on, which only the one running
/// session per connection may do; so nothing is attempted before save.
const FIX_NOT_TESTABLE: TestOutcome =
    TestOutcome::NotTestable("FIX credentials are validated at session logon, not before save");

/// Read a connector's typed credential, worded the way a failed connection
/// reports it. A credential of another kind is refused even when its fields
/// would fit — it is another venue's key.
fn typed<T: serde::de::DeserializeOwned>(creds: &BrokerCredentials, kind: &str) -> Result<T, String> {
    if creds.kind != kind {
        return Err(format!("a {} credential is not a {kind} credential", creds.kind));
    }
    creds.parse().map_err(|e| format!("credential fields do not match their kind: {e}"))
}

/// The `StreamRegistry` key for one connection's execution stream. Never a
/// connection code (those carry no ':'), so it cannot collide with a feed entry.
pub fn exec_stream_code(code: &str, env: &str) -> String {
    format!("{}-{}:exec", code.to_lowercase(), env.to_lowercase())
}

/// Every connector, in the order the cockpit lists them.
pub fn all() -> &'static [&'static dyn Connector] {
    &[&alpaca::Alpaca, &binance::Binance, &bybit::Bybit, &coinbase::Coinbase, &okx::Okx, &ibkr::Ibkr]
}

pub fn by_code(code: &str) -> Option<&'static dyn Connector> {
    all().iter().copied().find(|c| c.manifest().code == code)
}

pub fn by_credential_kind(kind: &str) -> Option<&'static dyn Connector> {
    all().iter().copied().find(|c| c.manifest().credential_kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /admin/venues` drives the cockpit's connection form; moving each
    /// venue's description into its connector must not change one byte of it.
    /// The golden file is `venues()`'s output captured before the move.
    #[test]
    fn venues_json_is_unchanged() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("testdata/venues.json")).unwrap();
        assert_eq!(serde_json::to_value(all().iter().map(|c| (c.manifest().venue)()).collect::<Vec<_>>()).unwrap(), golden);
    }

    #[test]
    fn every_public_feed_keeps_its_label() {
        let labels: Vec<&str> = all().iter().filter_map(|c| c.public_feed()).map(|f| f.label).collect();
        assert_eq!(labels, ["BINANCE/SPOT", "BYBIT/SPOT", "COINBASE/SPOT", "OKX/SPOT"]);
    }

    /// A credential stored under another broker's row (hand-imported into the
    /// wrong row) must not be used as this broker's key, even when its fields
    /// happen to fit: an OKX key has everything a Bybit key has.
    #[test]
    fn a_credential_of_another_kind_is_refused() {
        let okx = BrokerCredentials::from_typed(
            "Okx",
            &crate::connectors::okx::adapter::OkxCredentials { api_key: "k".into(), api_secret: "s".into(), passphrase: "p".into() },
        );
        let bybit = by_code("BYBIT").unwrap();
        assert!(bybit.universe_provider("PAPER", &okx).is_err());
    }

    #[test]
    fn codes_and_kinds_are_unique() {
        let mut codes: Vec<_> = all().iter().map(|c| c.manifest().code).collect();
        let mut kinds: Vec<_> = all().iter().map(|c| c.manifest().credential_kind).collect();
        let n = codes.len();
        codes.sort_unstable();
        codes.dedup();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!((codes.len(), kinds.len()), (n, n));
    }

    #[test]
    fn every_connector_is_found_by_its_code_and_kind() {
        for c in all() {
            let m = c.manifest();
            assert_eq!(by_code(m.code).unwrap().manifest().code, m.code);
            assert_eq!(by_credential_kind(m.credential_kind).unwrap().manifest().code, m.code);
            assert_eq!((m.venue)().code, m.code, "a manifest and its venue name the same broker");
        }
        assert!(by_code("NASDAQ").is_none());
    }
}
