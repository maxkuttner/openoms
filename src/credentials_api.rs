//! Turning a submitted credential form into a typed `BrokerCredentials`, and
//! (where it is cheap and safe) checking that it actually authenticates.
//!
//! This is the seam between an HTTP handler (task 3) and `credentials.rs`:
//! `parse_broker` is pure — no I/O, no `config::load()`, no database — so it
//! can be exercised with plain unit tests, and `test_broker` is the only part
//! of this module that reaches the network.
//!
//! Nothing outside `mod tests` calls into this module yet — the HTTP handlers
//! that will (task 3) do not exist. `allow(dead_code)` at module scope stands
//! in for that until then, rather than being sprinkled item by item.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::adapters::alpaca::AlpacaAdapter;
use crate::credentials::BrokerCredentials;

/// A submitted credential form: field name to raw string value, exactly as
/// an HTML form or a JSON object would hand it over. Untyped on purpose —
/// giving each broker its own typed request struct would just move the
/// "which fields did they actually send" question from here into task 3's
/// handler, where the merge rule would have to be reimplemented per broker.
pub struct CredentialSubmission {
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

/// Tests a credential the cheap way, where one exists.
///
/// Alpaca gets a real check: `GET /v2/account` is a lightweight, read-only,
/// sub-second call, so a wrong key fails loudly before it is ever saved.
///
/// IBKR and Binance are FIX. The only way to know a FIX credential is good is
/// to log on with it, and this process already owns the one FIX session per
/// connection (`reload.rs` — the same reason a FIX credential is not
/// hot-reloadable: the session owns an OS thread and a running logon, and a
/// second logon attempt against it here would collide with that session
/// rather than test anything). So this deliberately does not attempt one and
/// says so via `NotTestable`, rather than reporting a pass it did not earn.
pub async fn test_broker(creds: &BrokerCredentials) -> TestOutcome {
    match creds {
        BrokerCredentials::Alpaca { key, secret } => {
            let adapter = AlpacaAdapter::new(key.clone(), secret.clone(), "PAPER");
            match adapter.get_account().await {
                Ok(_) => TestOutcome::Passed,
                Err(e) => TestOutcome::Failed(e.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::BrokerCredentials;

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
}
