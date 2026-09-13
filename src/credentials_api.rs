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

use crate::adapters::alpaca::AlpacaAdapter;
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
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CredentialSubmission {
    #[serde(flatten)]
    pub fields: HashMap<String, String>,
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
        // ssl defaults to true — the same default `BrokerCredentials::IbkrFix`
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
    match kind {
        "ALPACA" => {
            let (existing_key, existing_secret) = match existing {
                Some(BrokerCredentials::Alpaca { key, secret }) => (Some(key.as_str()), Some(secret.as_str())),
                _ => (None, None),
            };
            Ok(BrokerCredentials::Alpaca {
                key: field(sub, "key", existing_key)?.to_string(),
                secret: field(sub, "secret", existing_secret)?.to_string(),
            })
        }
        "IBKR" => {
            let existing = match existing {
                Some(BrokerCredentials::IbkrFix { host, port, sender_comp_id, target_comp_id, password, ssl }) => {
                    Some((host.as_str(), *port, sender_comp_id.as_str(), target_comp_id.as_str(), password.as_str(), *ssl))
                }
                _ => None,
            };
            Ok(BrokerCredentials::IbkrFix {
                host: field(sub, "host", existing.map(|e| e.0))?.to_string(),
                port: parse_port(sub, existing.map(|e| e.1))?,
                sender_comp_id: field(sub, "sender_comp_id", existing.map(|e| e.2))?.to_string(),
                target_comp_id: field(sub, "target_comp_id", existing.map(|e| e.3))?.to_string(),
                password: field(sub, "password", existing.map(|e| e.4))?.to_string(),
                ssl: parse_ssl(sub, existing.map(|e| e.5))?,
            })
        }
        "BINANCE" => {
            let existing = match existing {
                Some(BrokerCredentials::BinanceFix { host, port, sender_comp_id, target_comp_id, api_key, private_key }) => {
                    Some((host.as_str(), *port, sender_comp_id.as_str(), target_comp_id.as_str(), api_key.as_str(), private_key.as_str()))
                }
                _ => None,
            };
            Ok(BrokerCredentials::BinanceFix {
                host: field(sub, "host", existing.map(|e| e.0))?.to_string(),
                port: parse_port(sub, existing.map(|e| e.1))?,
                sender_comp_id: field(sub, "sender_comp_id", existing.map(|e| e.2))?.to_string(),
                target_comp_id: field(sub, "target_comp_id", existing.map(|e| e.3))?.to_string(),
                api_key: field(sub, "api_key", existing.map(|e| e.4))?.to_string(),
                private_key: field(sub, "private_key", existing.map(|e| e.5))?.to_string(),
            })
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
    match creds {
        BrokerCredentials::Alpaca { key, secret } => {
            let adapter = AlpacaAdapter::new(key.clone(), secret.clone(), environment);
            match tokio::time::timeout(TEST_TIMEOUT, adapter.get_account()).await {
                Ok(Ok(_)) => TestOutcome::Passed,
                Ok(Err(e)) => TestOutcome::Failed(e.to_string()),
                Err(_) => timed_out("Alpaca"),
            }
        }
        BrokerCredentials::IbkrFix { .. } => {
            TestOutcome::NotTestable("FIX credentials are validated at session logon, not before save")
        }
        BrokerCredentials::BinanceFix { .. } => {
            TestOutcome::NotTestable("FIX credentials are validated at session logon, not before save")
        }
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
    use super::*;
    use crate::credentials::{BrokerCredentials, FeedCredentials};

    fn sub(pairs: &[(&str, &str)]) -> CredentialSubmission {
        CredentialSubmission {
            fields: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    fn existing_alpaca() -> BrokerCredentials {
        BrokerCredentials::Alpaca { key: "OLDKEY".into(), secret: "OLDSECRET".into() }
    }

    #[test]
    fn a_full_submission_replaces_everything() {
        match parse_broker("ALPACA", None, &sub(&[("key", "NEWKEY"), ("secret", "NEWSECRET")])).expect("parse") {
            BrokerCredentials::Alpaca { key, secret } => {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "NEWSECRET");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The core of the edit experience: change the key id, leave the secret
    /// blank, keep the stored secret. Without this an operator cannot edit any
    /// non-secret field without re-typing the secret they cannot see.
    #[test]
    fn an_omitted_secret_keeps_the_stored_one() {
        match parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY")])).expect("parse") {
            BrokerCredentials::Alpaca { key, secret } => {
                assert_eq!(key, "NEWKEY");
                assert_eq!(secret, "OLDSECRET", "the stored secret must survive an omission");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// An empty string is an omission, not a value. Treating "" as "set the
    /// secret to empty" would silently break a working connection.
    #[test]
    fn an_empty_secret_is_treated_as_omitted() {
        match parse_broker("ALPACA", Some(&existing_alpaca()), &sub(&[("key", "NEWKEY"), ("secret", "")])).expect("parse") {
            BrokerCredentials::Alpaca { secret, .. } => assert_eq!(secret, "OLDSECRET"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn existing_ibkr() -> BrokerCredentials {
        BrokerCredentials::IbkrFix {
            host: "old.fix.example".into(),
            port: 4101,
            sender_comp_id: "OLDSENDER".into(),
            target_comp_id: "OLDTARGET".into(),
            password: "OLDPASSWORD".into(),
            ssl: true,
        }
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
            BrokerCredentials::IbkrFix { host, .. } => {
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
            BrokerCredentials::IbkrFix { host, .. } => {
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
