//! Connectors: one module per broker, each describing itself with a [`Manifest`].
//!
//! The central parts of the OMS — the connection form, credential handling,
//! registration, stream restarts — read what they need from [`all()`] instead of
//! naming brokers. Adding one is its module here, its line in `all()`, its
//! migration and its docs page.

use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::adapters::BrokerAdapter;
use crate::credentials::{BrokerCredentials, Shown};
use crate::order_engine::EngineHandle;
use crate::reload::RegistrationDeps;
use crate::stream_health::StreamHandle;
use crate::venues::Venue;

mod alpaca;
mod binance;
mod bybit;
mod coinbase;
mod ibkr;
mod okx;

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

/// Starts one execution stream; called again on every restart.
pub type ExecStream = Arc<dyn Fn(StreamHandle, EngineHandle) -> BoxFuture<'static, ()> + Send + Sync>;

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
}

/// Read a connector's typed credential, worded the way a failed connection reports it.
fn typed<T: serde::de::DeserializeOwned>(creds: &BrokerCredentials) -> Result<T, String> {
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
        assert_eq!(serde_json::to_value(crate::venues::venues()).unwrap(), golden);
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
