//! Turning a submitted credential form into a typed `BrokerCredentials`, and
//! (where it is cheap and safe) checking that it actually authenticates.
//!
//! This is the seam between an HTTP handler (`admin.rs`'s
//! `put`/`delete`/`test` broker-credential endpoints) and `credentials.rs`:
//! `parse_broker` is pure — no I/O, no `config::load()`, no database — so it
//! can be exercised with plain unit tests, and `test_broker` is the only part
//! of this module that reaches the network.

use std::collections::HashMap;
use std::time::Duration;
use serde::Deserialize;

use crate::adapters::alpaca::{AlpacaAdapter, AlpacaCredentials};
use crate::adapters::binance::BinanceCredentials;
use crate::adapters::bybit::{BybitAdapter, BybitCredentials};
use crate::adapters::coinbase::{CoinbaseAdapter, CoinbaseCredentials};
use crate::adapters::okx::{OkxAdapter, OkxCredentials};
use crate::fix::ibkr::IbkrFixCredentials;
use crate::credentials::{BrokerCredentials, FeedCredentials};

/// A submitted credential form: field name to raw string value, exactly as
/// an HTML form or a JSON object would hand it over. Untyped on purpose —
/// giving each broker its own typed request struct would just move the
/// "which fields did they actually send" question from here into the
/// handler, where the merge rule would have to be reimplemented per broker.
///
/// `#[serde(flatten)]` so the wire body is the field map itself
/// (`{"host": "...", "port": "4101"}`), not `{"fields": {...}}` — the shape
/// the cockpit form naturally produces.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CredentialSubmission {
    #[serde(flatten)]
    pub fields: HashMap<String, String>,
}

/// Hand-written for the same reason `BrokerCredentials` and `FeedCredentials`
/// have their own: this holds raw submitted secrets, so a derived `Debug` is
/// one `info!(?submission)` away from putting every one of them in the log.
/// Prints the field *names* — useful for seeing what a form actually sent —
/// and never a value.
impl std::fmt::Debug for CredentialSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.fields.keys().map(String::as_str).collect();
        names.sort_unstable(); // HashMap order is arbitrary; a stable Debug is easier to read
        write!(f, "CredentialSubmission{names:?}")
    }
}

impl CredentialSubmission {
    /// The field's value, unless it was omitted or submitted empty — the two
    /// cases the merge rule treats identically. `HashMap::get` alone would
    /// hand back `Some("")` for an empty secret, so callers that did their
    /// own `.get` instead of going through this would have to remember the
    /// merge rule twice.
    fn get(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }
}

/// Why `parse_broker` could not produce a credential.
///
/// `why` fields are hand-written descriptions of the *shape* of the problem
/// ("not a valid port number"), never anything derived from the submitted
/// value — see `no_error_message_echoes_a_submitted_value` in `mod tests`.
/// These reach an operator's screen and the server log, so a value that
/// failed to parse must never ride along in the message that reports it.
#[derive(Debug)]
pub enum CredentialError {
    /// `kind` was not one of the broker codes this module knows how to parse.
    UnknownKind(String),
    /// A required field was neither submitted nor available to carry over
    /// from an existing credential. The name is a fixed field name, not
    /// submitted input, so it is safe to include directly.
    MissingField(&'static str),
    /// A field was submitted but could not be turned into the type the
    /// credential needs (e.g. a non-numeric port).
    BadField { name: &'static str, why: String },
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::UnknownKind(kind) => write!(f, "unknown broker kind {kind:?}"),
            CredentialError::MissingField(name) => write!(f, "missing required field {name:?}"),
            CredentialError::BadField { name, why } => write!(f, "field {name:?}: {why}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// A field that may be carried over from `existing` when the submission
/// omits it (or submits it empty). Non-secret fields and secret fields both
/// go through this — the difference between them is entirely in which
/// `existing` accessor the call site passes, not in the merge logic itself.
fn field<'a>(
    sub: &'a CredentialSubmission,
    name: &'static str,
    existing: Option<&'a str>,
) -> Result<&'a str, CredentialError> {
    match sub.get(name) {
        Some(v) => Ok(v),
        None => existing.ok_or(CredentialError::MissingField(name)),
    }
}

fn parse_port(sub: &CredentialSubmission, existing: Option<u16>) -> Result<u16, CredentialError> {
    match sub.get("port") {
        Some(v) => v.parse::<u16>().map_err(|_| CredentialError::BadField {
            name: "port",
            why: "not a valid port number".into(),
        }),
        None => existing.ok_or(CredentialError::MissingField("port")),
    }
}

fn parse_ssl(sub: &CredentialSubmission, existing: Option<bool>) -> Result<bool, CredentialError> {
    match sub.get("ssl") {
        Some(v) => v.parse::<bool>().map_err(|_| CredentialError::BadField {
            name: "ssl",
            why: "not a valid boolean (expected \"true\" or \"false\")".into(),
        }),
        // ssl defaults to true — the same default `IbkrFixCredentials`
        // applies on deserialize — when nothing is submitted or stored yet.
        None => Ok(existing.unwrap_or(true)),
    }
}

/// Turns a submitted form into a typed credential, merging with `existing`
/// per the rule above: a present, non-empty value replaces; an omitted or
/// empty value keeps what is stored; omitting a value with nothing stored is
/// `MissingField`.
pub fn parse_broker(
    kind: &str,
    existing: Option<&BrokerCredentials>,
    sub: &CredentialSubmission,
) -> Result<BrokerCredentials, CredentialError> {
    // the stored credential's own typed view, when it is of the submitted kind
    fn stored<T: serde::de::DeserializeOwned>(existing: Option<&BrokerCredentials>, kind: &str) -> Option<T> {
        existing.filter(|c| c.kind == kind).and_then(|c| c.parse().ok())
    }
    match kind {
        "ALPACA" => {
            let e = stored::<AlpacaCredentials>(existing, AlpacaCredentials::KIND);
            let c = AlpacaCredentials {
                key: field(sub, "key", e.as_ref().map(|e| e.key.as_str()))?.to_string(),
                secret: field(sub, "secret", e.as_ref().map(|e| e.secret.as_str()))?.to_string(),
            };
            Ok(BrokerCredentials::from_typed(AlpacaCredentials::KIND, &c))
        }
        "IBKR" => {
            let e = stored::<IbkrFixCredentials>(existing, IbkrFixCredentials::KIND);
            let c = IbkrFixCredentials {
                host: field(sub, "host", e.as_ref().map(|e| e.host.as_str()))?.to_string(),
                port: parse_port(sub, e.as_ref().map(|e| e.port))?,
                sender_comp_id: field(sub, "sender_comp_id", e.as_ref().map(|e| e.sender_comp_id.as_str()))?.to_string(),
                target_comp_id: field(sub, "target_comp_id", e.as_ref().map(|e| e.target_comp_id.as_str()))?.to_string(),
                password: field(sub, "password", e.as_ref().map(|e| e.password.as_str()))?.to_string(),
                ssl: parse_ssl(sub, e.as_ref().map(|e| e.ssl))?,
            };
            Ok(BrokerCredentials::from_typed(IbkrFixCredentials::KIND, &c))
        }
        "BINANCE" => {
            let e = stored::<BinanceCredentials>(existing, BinanceCredentials::KIND);
            let c = BinanceCredentials {
                host: field(sub, "host", e.as_ref().map(|e| e.host.as_str()))?.to_string(),
                port: parse_port(sub, e.as_ref().map(|e| e.port))?,
                sender_comp_id: field(sub, "sender_comp_id", e.as_ref().map(|e| e.sender_comp_id.as_str()))?.to_string(),
                target_comp_id: field(sub, "target_comp_id", e.as_ref().map(|e| e.target_comp_id.as_str()))?.to_string(),
                api_key: field(sub, "api_key", e.as_ref().map(|e| e.api_key.as_str()))?.to_string(),
                private_key: field(sub, "private_key", e.as_ref().map(|e| e.private_key.as_str()))?.to_string(),
            };
            Ok(BrokerCredentials::from_typed(BinanceCredentials::KIND, &c))
        }
        "BYBIT" => {
            let e = stored::<BybitCredentials>(existing, BybitCredentials::KIND);
            let c = BybitCredentials {
                api_key: field(sub, "api_key", e.as_ref().map(|e| e.api_key.as_str()))?.to_string(),
                api_secret: field(sub, "api_secret", e.as_ref().map(|e| e.api_secret.as_str()))?.to_string(),
            };
            Ok(BrokerCredentials::from_typed(BybitCredentials::KIND, &c))
        }
        "COINBASE" => {
            let e = stored::<CoinbaseCredentials>(existing, CoinbaseCredentials::KIND);
            let c = CoinbaseCredentials {
                api_key: field(sub, "api_key", e.as_ref().map(|e| e.api_key.as_str()))?.to_string(),
                private_key: field(sub, "private_key", e.as_ref().map(|e| e.private_key.as_str()))?.to_string(),
            };
            Ok(BrokerCredentials::from_typed(CoinbaseCredentials::KIND, &c))
        }
        "OKX" => {
            let e = stored::<OkxCredentials>(existing, OkxCredentials::KIND);
            let c = OkxCredentials {
                api_key: field(sub, "api_key", e.as_ref().map(|e| e.api_key.as_str()))?.to_string(),
                api_secret: field(sub, "api_secret", e.as_ref().map(|e| e.api_secret.as_str()))?.to_string(),
                passphrase: field(sub, "passphrase", e.as_ref().map(|e| e.passphrase.as_str()))?.to_string(),
            };
            Ok(BrokerCredentials::from_typed(OkxCredentials::KIND, &c))
        }
        other => Err(CredentialError::UnknownKind(other.to_string())),
    }
}

/// Turns a submitted form into a typed feed credential, same merge rule as
/// `parse_broker` above (present-non-empty replaces; omitted-or-empty keeps
/// what is stored; omitted with nothing stored is `MissingField`).
///
/// `FeedCredentials` has exactly one variant today, but this is written as a
/// `match kind` — not a bare struct literal — so a second provider is a new
/// arm here, the same shape `parse_broker` already has for its three
/// brokers, rather than a rewrite of this function's signature.
pub fn parse_feed(
    kind: &str,
    existing: Option<&FeedCredentials>,
    sub: &CredentialSubmission,
) -> Result<FeedCredentials, CredentialError> {
    match kind {
        "DATABENTO" => {
            let existing_key =
                existing.map(|FeedCredentials::Databento { api_key }| api_key.as_str());
            Ok(FeedCredentials::Databento { api_key: field(sub, "api_key", existing_key)?.to_string() })
        }
        other => Err(CredentialError::UnknownKind(other.to_string())),
    }
}

/// Outcome of attempting to verify a credential before it is saved.
///
/// A plain `Result<(), String>` can only say "it worked" or "it didn't" — it
/// cannot also say "we didn't try", so a FIX credential would have to fake one
/// of the other two. Faking `Ok(())` is the dishonest "passed" the brief warns
/// against; faking `Err(_)` would show a real credential as broken before it
/// is even saved. A third variant is the only way to keep both meanings
/// distinct for the caller (task 3's `tested` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    /// The credential authenticated against the broker.
    Passed,
    /// The credential was tested and the broker rejected it (or the attempt
    /// failed for a network reason). `String` is a message for operators and
    /// logs — built from the failure, never from the submitted credential.
    Failed(String),
    /// Nothing was attempted. Reserved for credentials with no cheap,
    /// side-effect-free way to test.
    NotTestable(&'static str),
}

/// How long a credential test may spend talking to a provider before it is
/// reported as a failure.
///
/// Neither provider client sets a timeout of its own (Alpaca's is a bare
/// `reqwest::Client::new()`, which has none by default), so without this a
/// provider that completes the TCP handshake and then goes silent would hold
/// the admin request open indefinitely — and with it the cockpit's "Test"
/// button, which has no way to cancel. Ten seconds is far longer than either
/// check needs against a healthy provider.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The failure reported when a provider stops responding mid-test. Says which
/// provider and how long was waited, because "test failed" alone would be
/// indistinguishable from a rejected credential — the operator needs to know
/// their key was never actually judged.
fn timed_out(provider: &str) -> TestOutcome {
    TestOutcome::Failed(format!(
        "{provider} did not respond within {}s — credential not verified",
        TEST_TIMEOUT.as_secs()
    ))
}

/// Tests a credential the cheap way, where one exists.
///
/// Alpaca gets a real check: `GET /v2/account` is a lightweight, read-only,
/// sub-second call, so a wrong key fails loudly before it is ever saved.
/// `environment` ("PAPER" | "LIVE") must be the *connection's own*
/// environment, not a hardcoded literal — `AlpacaAdapter::new` picks the live
/// endpoint only for exactly `"LIVE"`, so a real live credential tested
/// against a hardcoded `"PAPER"` would 401 and the caller's gate would
/// refuse a credential that is perfectly valid.
///
/// IBKR and Binance are FIX. The only way to know a FIX credential is good is
/// to log on with it, and this process already owns the one FIX session per
/// connection (`reload.rs` — the same reason a FIX credential is not
/// hot-reloadable: the session owns an OS thread and a running logon, and a
/// second logon attempt against it here would collide with that session
/// rather than test anything). So this deliberately does not attempt one and
/// says so via `NotTestable`, rather than reporting a pass it did not earn.
pub async fn test_broker(creds: &BrokerCredentials, environment: &str) -> TestOutcome {
    let unreadable = |e: serde_json::Error| TestOutcome::Failed(format!("credential fields do not match their kind: {e}"));
    match creds.kind.as_str() {
        AlpacaCredentials::KIND => {
            let c = match creds.parse::<AlpacaCredentials>() { Ok(c) => c, Err(e) => return unreadable(e) };
            let adapter = AlpacaAdapter::new(c.key, c.secret, environment);
            match tokio::time::timeout(TEST_TIMEOUT, adapter.get_account()).await {
                Ok(Ok(_)) => TestOutcome::Passed,
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("Alpaca"),
            }
        }
        IbkrFixCredentials::KIND | BinanceCredentials::KIND => {
            TestOutcome::NotTestable("FIX credentials are validated at session logon, not before save")
        }
        CoinbaseCredentials::KIND => {
            let c = match creds.parse::<CoinbaseCredentials>() { Ok(c) => c, Err(e) => return unreadable(e) };
            let adapter = match CoinbaseAdapter::new(c.api_key, &c.private_key) {
                Ok(a) => a,
                Err(e) => return TestOutcome::Failed(e),
            };
            match tokio::time::timeout(TEST_TIMEOUT, adapter.key_permissions()).await {
                Ok(Ok(permissions)) => judge_coinbase_permissions(&permissions),
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("Coinbase"),
            }
        }
        BybitCredentials::KIND => {
            let c = match creds.parse::<BybitCredentials>() { Ok(c) => c, Err(e) => return unreadable(e) };
            let adapter = BybitAdapter::new(c.api_key, c.api_secret, environment);
            match tokio::time::timeout(TEST_TIMEOUT, adapter.api_key_info()).await {
                Ok(Ok(_)) => TestOutcome::Passed,
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("Bybit"),
            }
        }
        OkxCredentials::KIND => {
            let c = match creds.parse::<OkxCredentials>() { Ok(c) => c, Err(e) => return unreadable(e) };
            let adapter = OkxAdapter::new(c.api_key, c.api_secret, c.passphrase, environment);
            match tokio::time::timeout(TEST_TIMEOUT, adapter.key_permissions()).await {
                Ok(Ok(perm)) => judge_okx_permissions(&perm),
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("OKX"),
            }
        }
        other => TestOutcome::Failed(format!("no connector for credential kind {other}")),
    }
}

/// An OKX key is fit for the OMS when it can trade and cannot withdraw — the
/// same rule as Coinbase. `perm` is a comma list such as "read_only,trade".
pub(crate) fn judge_okx_permissions(perm: &str) -> TestOutcome {
    let has = |p: &str| perm.split(',').any(|x| x.trim() == p);
    if has("withdraw") {
        TestOutcome::Failed("the key can withdraw funds; create one with Read and Trade permissions only".into())
    } else if !has("trade") {
        TestOutcome::Failed("the key cannot trade; give it the Trade permission".into())
    } else {
        TestOutcome::Passed
    }
}

/// A Coinbase key is fit for the OMS when it can trade and cannot move money
/// out: an OMS never needs to withdraw, so a transfer-capable key is refused.
pub(crate) fn judge_coinbase_permissions(permissions: &serde_json::Value) -> TestOutcome {
    if permissions["can_transfer"].as_bool() != Some(false) {
        TestOutcome::Failed("the key can transfer funds; create one with View and Trade permissions only".into())
    } else if permissions["can_trade"].as_bool() != Some(true) {
        TestOutcome::Failed("the key cannot trade; give it the Trade permission".into())
    } else {
        TestOutcome::Passed
    }
}

/// The Databento dataset `test_feed` authenticates against. Mirrors
/// `opra_stream::OPRA_DATASET` (private to that module, so not reusable
/// directly): `databento-opra` is, today, the only Databento feed connection
/// this build ever registers (see `admin::classify_feed`'s and `serve()`'s
/// matching "only databento-opra" guards) and it is hardcoded to OPRA options
/// data. A second Databento feed on a different dataset would need this
/// hardcoded literal replaced with something read off the connection row,
/// not a new parameter threaded through just for this one caller.
const DATABENTO_OPRA_DATASET: &str = "OPRA.PILLAR";

/// Tests a feed credential the cheap way, where one exists — the feed-side
/// counterpart to `test_broker`.
///
/// Databento's live gateway is authenticated as part of connecting:
/// `LiveClient::builder().key(..).dataset(..).build()` opens a TCP connection
/// and runs the CRAM handshake, and returns an error if the gateway rejects
/// the key — before a single `subscribe()` or `start()` is ever called, so
/// this never requests, receives, or decodes a market-data record. That
/// makes it the same shape of check as Alpaca's `GET /v2/account`: a real,
/// lightweight, side-effect-free round trip against the provider, not a full
/// live subscription. The connection is closed immediately after a
/// successful build; nothing about it is kept.
pub async fn test_feed(creds: &FeedCredentials) -> TestOutcome {
    match creds {
        FeedCredentials::Databento { api_key } => {
            // Screened before the key reaches the client, not for validation's
            // sake but to keep it out of the log. `databento`'s `ApiKey::new`
            // (0.54.0, src/lib.rs:249) reacts to a 32-character key containing
            // any non-ASCII byte with `error!("API key '{key}' ...")` — the
            // whole key, into our subscriber. The returned error's Display is
            // clean, so only the log leaks, but a credential in a log file is
            // exactly what this module exists to prevent. The realistic
            // trigger is a paste whose length is right and one of whose
            // characters arrived as a Unicode lookalike — a smart quote or an
            // en-dash out of a chat client.
            if !api_key.is_ascii() {
                return TestOutcome::Failed(
                    "key contains non-ASCII characters — it was probably pasted from an \
                     editor or chat client that substituted a character"
                        .to_string(),
                );
            }
            let builder = match databento::LiveClient::builder().key(api_key.clone()) {
                Ok(b) => b,
                Err(e) => return TestOutcome::Failed(e.to_string()),
            };
            let build = builder.dataset(DATABENTO_OPRA_DATASET).build();
            match tokio::time::timeout(TEST_TIMEOUT, build).await {
                Ok(Ok(mut client)) => {
                    // Best-effort, and bounded for the same reason the build
                    // is: the auth check already happened in `build()` above,
                    // so failing to close politely does not change the
                    // outcome — it only leaves the gateway to time the
                    // connection out on its own.
                    let _ = tokio::time::timeout(TEST_TIMEOUT, client.close()).await;
                    TestOutcome::Passed
                }
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("Databento"),
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// A non-ASCII key must be rejected *before* it reaches the databento
    /// client, which logs the whole key when handed a 32-character key with a
    /// non-ASCII byte. Returns early, so this test touches no network.
    #[tokio::test]
    async fn a_non_ascii_databento_key_is_rejected_without_reaching_the_client() {
        // 32 chars, one of them a Unicode en-dash — the length that reaches
        // databento's non-ASCII branch.
        let key = format!("{}\u{2013}", "a".repeat(31));
        assert_eq!(key.chars().count(), 32);
        let outcome = test_feed(&FeedCredentials::Databento { api_key: key.clone() }).await;
        match outcome {
            TestOutcome::Failed(msg) => {
                assert!(msg.contains("non-ASCII"), "unexpected message: {msg}");
                assert!(!msg.contains(&key), "the failure echoed the key back");
                assert!(!msg.contains(&"a".repeat(31)), "the failure echoed the key back");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// `CredentialSubmission` holds raw secrets, so its `Debug` must print
    /// names only. A derived `Debug` here would put every submitted secret one
    /// `info!(?submission)` away from the log.
    #[test]
    fn debug_for_a_submission_never_prints_a_value() {
        let s = sub(&[("key", "AKREALKEY"), ("secret", "sup3r-s3cret")]);
        let printed = format!("{s:?}");
        assert!(printed.contains("key"), "field names are useful: {printed}");
        assert!(printed.contains("secret"));
        assert!(!printed.contains("AKREALKEY"), "leaked a value: {printed}");
        assert!(!printed.contains("sup3r-s3cret"), "leaked a value: {printed}");
    }
    use super::*;

    #[test]
    fn an_okx_key_must_trade_and_must_not_withdraw() {
        assert_eq!(judge_okx_permissions("read_only,trade"), TestOutcome::Passed);
        assert!(matches!(judge_okx_permissions("read_only,withdraw,trade"), TestOutcome::Failed(m) if m.contains("withdraw")));
        assert!(matches!(judge_okx_permissions("read_only"), TestOutcome::Failed(m) if m.contains("trade")));
    }

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
    use crate::credentials::{BrokerCredentials, FeedCredentials};

    fn sub(pairs: &[(&str, &str)]) -> CredentialSubmission {
        CredentialSubmission {
            fields: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn existing_alpaca() -> BrokerCredentials {
        BrokerCredentials::from_typed("Alpaca", &crate::adapters::alpaca::AlpacaCredentials { key: "OLDKEY".into(), secret: "OLDSECRET".into() })
    }

    #[test]
    fn a_full_submission_replaces_everything() {
        {
            let c = parse_broker("ALPACA", None, &sub(&[("key", "NEWKEY"), ("secret", "NEWSECRET")])).expect("parse");
            assert_eq!(c.kind, "Alpaca", "wrong kind: {c:?}");
            let crate::adapters::alpaca::AlpacaCredentials { key, secret } = c.parse::<crate::adapters::alpaca::AlpacaCredentials>().unwrap();
            {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "NEWSECRET");
            }
        }
    }

    /// The core of the edit experience: change the key id, leave the secret
    /// blank, keep the stored secret. Without this an operator cannot edit any
    /// non-secret field without re-typing the secret they cannot see.
    #[test]
    fn an_omitted_secret_keeps_the_stored_one() {
        {
            let c = parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY")])).expect("parse");
            assert_eq!(c.kind, "Alpaca", "wrong kind: {c:?}");
            let crate::adapters::alpaca::AlpacaCredentials { key, secret } = c.parse::<crate::adapters::alpaca::AlpacaCredentials>().unwrap();
            {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "OLDSECRET", "the stored secret must survive an omission");
            }
        }
    }

    /// An empty string is an omission, not a value. Treating "" as "set the
    /// secret to empty" would silently break a working connection.
    #[test]
    fn an_empty_secret_is_treated_as_omitted() {
        {
            let c = parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY"), ("secret", "")])).expect("parse");
            assert_eq!(c.kind, "Alpaca", "wrong kind: {c:?}");
            let crate::adapters::alpaca::AlpacaCredentials { secret, .. } = c.parse::<crate::adapters::alpaca::AlpacaCredentials>().unwrap();
            { assert_eq!(secret, "OLDSECRET"); }
        }
    }

    fn existing_ibkr() -> BrokerCredentials {
        BrokerCredentials::from_typed("IbkrFix", &crate::fix::ibkr::IbkrFixCredentials {
            host: "old.fix.example".into(),
            port: 4101,
            sender_comp_id: "OLDSENDER".into(),
            target_comp_id: "OLDTARGET".into(),
            password: "OLDPASSWORD".into(),
            ssl: true,
        })
    }

    /// The merge rule above is proven only for Alpaca's two fields; IBKR and
    /// Binance have six each, verified so far only by hand-trace (task 2
    /// review). This exercises it on a non-secret IBKR field: omit `host`,
    /// submit everything else, and confirm the stored host survives. A
    /// regression here would silently inherit an old FIX host.
    #[test]
    fn an_omitted_non_secret_field_keeps_the_stored_value_on_ibkr() {
        match parse_broker(
            "IBKR",
            Some(&existing_ibkr()),
            &sub(&[
                ("port", "4102"),
                ("sender_comp_id", "NEWSENDER"),
                ("target_comp_id", "NEWTARGET"),
                ("password", "NEWPASSWORD"),
            ]),
        )
        .expect("parse")
        {
            c if c.kind == "IbkrFix" => {
                let crate::fix::ibkr::IbkrFixCredentials { host, .. } = c.parse::<crate::fix::ibkr::IbkrFixCredentials>().unwrap();
                assert_eq!(host, "old.fix.example", "an omitted host must carry over from the stored credential");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Same rule, empty-string submission: a blank `host` input left in a
    /// form must not silently blank out a working FIX host.
    #[test]
    fn an_empty_non_secret_field_keeps_the_stored_value_on_ibkr() {
        match parse_broker(
            "IBKR",
            Some(&existing_ibkr()),
            &sub(&[
                ("host", ""),
                ("port", "4102"),
                ("sender_comp_id", "NEWSENDER"),
                ("target_comp_id", "NEWTARGET"),
                ("password", "NEWPASSWORD"),
            ]),
        )
        .expect("parse")
        {
            c if c.kind == "IbkrFix" => {
                let crate::fix::ibkr::IbkrFixCredentials { host, .. } = c.parse::<crate::fix::ibkr::IbkrFixCredentials>().unwrap();
                assert_eq!(host, "old.fix.example", "an empty host must be treated as omitted, not as a value");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_omitted_secret_with_nothing_stored_is_an_error() {
        assert!(matches!(
            parse_broker("ALPACA", None, &sub(&[("key", "NEWKEY")])),
            Err(CredentialError::MissingField("secret"))
        ));
    }

    #[test]
    fn an_unparseable_port_is_a_field_error_not_a_panic() {
        let r = parse_broker("IBKR", None, &sub(&[
            ("host", "h"), ("port", "not-a-number"), ("password", "p"),
        ]));
        assert!(matches!(r, Err(CredentialError::BadField { name: "port", .. })));
    }

    #[test]
    fn an_unknown_broker_kind_is_rejected() {
        assert!(matches!(parse_broker("NASDAQ", None, &sub(&[])), Err(CredentialError::UnknownKind(_))));
    }

    /// THE regression that matters: an error message is shown to a user and
    /// logged. It must never contain what they submitted.
    #[test]
    fn no_error_message_echoes_a_submitted_value() {
        let r = parse_broker("IBKR", None, &sub(&[
            ("host", "h"), ("port", "SUPERSECRETVALUE"), ("password", "SUPERSECRETVALUE"),
        ]));
        let msg = format!("{:?}", r.unwrap_err());
        assert!(!msg.contains("SUPERSECRETVALUE"), "submitted value leaked into {msg}");
    }

    // ── parse_feed ──────────────────────────────────────────────────────

    fn existing_databento() -> FeedCredentials {
        FeedCredentials::Databento { api_key: "OLDKEY".into() }
    }

    #[test]
    fn a_full_feed_submission_replaces_everything() {
        match parse_feed("DATABENTO", None, &sub(&[("api_key", "NEWKEY")])).expect("parse") {
            FeedCredentials::Databento { api_key } => assert_eq!(api_key, "NEWKEY"),
        }
    }

    /// The same edit experience `parse_broker` gives brokers: an omitted
    /// secret must inherit the stored one rather than erroring or blanking it.
    #[test]
    fn an_omitted_feed_secret_keeps_the_stored_one() {
        match parse_feed("DATABENTO", Some(&existing_databento()), &sub(&[])).expect("parse") {
            FeedCredentials::Databento { api_key } => {
                assert_eq!(api_key, "OLDKEY", "the stored key must survive an omission");
            }
        }
    }

    /// An empty string is an omission, not a value — same rule as brokers.
    #[test]
    fn an_empty_feed_secret_is_treated_as_omitted() {
        match parse_feed("DATABENTO", Some(&existing_databento()), &sub(&[("api_key", "")])).expect("parse") {
            FeedCredentials::Databento { api_key } => assert_eq!(api_key, "OLDKEY"),
        }
    }

    #[test]
    fn an_omitted_feed_secret_with_nothing_stored_is_an_error() {
        assert!(matches!(
            parse_feed("DATABENTO", None, &sub(&[])),
            Err(CredentialError::MissingField("api_key"))
        ));
    }

    #[test]
    fn an_unknown_feed_kind_is_rejected() {
        assert!(matches!(parse_feed("POLYGON", None, &sub(&[])), Err(CredentialError::UnknownKind(_))));
    }
}
