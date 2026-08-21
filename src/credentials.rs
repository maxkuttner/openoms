//! What a connection needs in order to authenticate, and what may be shown of it.
//!
//! Knows nothing about AES — sealing is `crate::secrets`' job. This module owns
//! the shapes, their JSON encoding, and the redacted view that is safe to put on
//! the wire.

use serde::{Deserialize, Serialize};

/// Credentials for one broker connection, tagged by broker so a blob read back
/// from the database identifies itself.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum BrokerCredentials {
    Alpaca {
        key: String,
        secret: String,
    },
    IbkrFix {
        host: String,
        port: u16,
        sender_comp_id: String,
        target_comp_id: String,
        password: String,
        #[serde(default = "default_true")]
        ssl: bool,
    },
    BinanceFix {
        host: String,
        port: u16,
        sender_comp_id: String,
        target_comp_id: String,
        api_key: String,
        /// PEM contents, not a path. A filesystem path is precisely what stops a
        /// credential being configurable from anywhere but the server's shell.
        private_key: String,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum FeedCredentials {
    Databento { api_key: String },
}

/// A credential as it may be shown: non-secret fields in the clear, secret fields
/// present by name with no value. Ordered so the UI can render it directly.
#[derive(Debug, Serialize)]
pub struct Redacted {
    pub fields: Vec<(String, Option<String>)>,
}

pub trait Redact {
    fn redact(&self) -> Redacted;
}

fn shown(name: &str, value: impl ToString) -> (String, Option<String>) {
    (name.to_string(), Some(value.to_string()))
}

/// A secret: named so the UI knows the field exists, with no value.
fn hidden(name: &str) -> (String, Option<String>) {
    (name.to_string(), None)
}

impl Redact for BrokerCredentials {
    fn redact(&self) -> Redacted {
        let fields = match self {
            BrokerCredentials::Alpaca { key, .. } => vec![
                // The last four characters identify which key is installed without
                // being enough to use it — the same trick every payment UI uses.
                shown("key", format!("…{}", tail4(key))),
                hidden("secret"),
            ],
            BrokerCredentials::IbkrFix { host, port, sender_comp_id, target_comp_id, ssl, .. } => vec![
                shown("host", host),
                shown("port", port),
                shown("sender_comp_id", sender_comp_id),
                shown("target_comp_id", target_comp_id),
                shown("ssl", ssl),
                hidden("password"),
            ],
            BrokerCredentials::BinanceFix { host, port, sender_comp_id, target_comp_id, .. } => vec![
                shown("host", host),
                shown("port", port),
                shown("sender_comp_id", sender_comp_id),
                shown("target_comp_id", target_comp_id),
                hidden("api_key"),
                hidden("private_key"),
            ],
        };
        Redacted { fields }
    }
}

impl Redact for FeedCredentials {
    fn redact(&self) -> Redacted {
        Redacted { fields: vec![hidden("api_key")] }
    }
}

/// Last 4 characters of `s`, or nothing at all when `s` is too short for that to
/// mean anything. Without this floor, a 1-4 character key would come back through
/// `redact()` whole — the opposite of what "not enough to use it" promises.
fn tail4(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return String::new();
    }
    chars[chars.len() - 4..].iter().collect()
}

// Hand-written for both enums: deriving Debug would print every secret they hold
// the first time one appears in a panic message or a log line.
impl std::fmt::Debug for BrokerCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BrokerCredentials{:?}", self.redact().fields)
    }
}

impl std::fmt::Debug for FeedCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FeedCredentials{:?}", self.redact().fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpaca() -> BrokerCredentials {
        BrokerCredentials::Alpaca { key: "AKTESTKEY123".into(), secret: "SUPERSECRETVALUE".into() }
    }

    fn binance() -> BrokerCredentials {
        BrokerCredentials::BinanceFix {
            host: "fix-oe.testnet.binance.vision".into(),
            port: 9000,
            sender_comp_id: "OMS".into(),
            target_comp_id: "SPOT".into(),
            api_key: "BNKEY999".into(),
            private_key: "-----BEGIN PRIVATE KEY-----\nMIIBSECRET\n-----END PRIVATE KEY-----".into(),
        }
    }

    fn ibkr() -> BrokerCredentials {
        BrokerCredentials::IbkrFix {
            host: "ibkr-gateway.example.com".into(),
            port: 4001,
            sender_comp_id: "OMSIB".into(),
            target_comp_id: "IBKRTARGET".into(),
            password: "IBKRPASSWORD777".into(),
            // Explicitly false so the round-trip test can catch the serde default
            // silently overriding a stored value.
            ssl: false,
        }
    }

    /// The tag is what tells us which broker a blob belongs to when it comes back
    /// out of the database, so it must survive the round trip exactly.
    #[test]
    fn json_round_trips_every_variant() {
        // Assert on the secret VALUES, not on Debug output: Debug is redacted, so
        // comparing rendered strings would pass even if a secret were lost in the
        // round trip — the one thing this test exists to catch.
        let json = serde_json::to_vec(&alpaca()).expect("serialize");
        match serde_json::from_slice::<BrokerCredentials>(&json).expect("deserialize") {
            BrokerCredentials::Alpaca { key, secret } => {
                assert_eq!(key, "AKTESTKEY123");
                assert_eq!(secret, "SUPERSECRETVALUE");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let json = serde_json::to_vec(&binance()).expect("serialize");
        match serde_json::from_slice::<BrokerCredentials>(&json).expect("deserialize") {
            BrokerCredentials::BinanceFix { host, port, api_key, private_key, target_comp_id, .. } => {
                assert_eq!(host, "fix-oe.testnet.binance.vision");
                assert_eq!(port, 9000);
                assert_eq!(target_comp_id, "SPOT");
                assert_eq!(api_key, "BNKEY999");
                assert!(private_key.contains("MIIBSECRET"), "PEM body must survive intact");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let json = serde_json::to_vec(&ibkr()).expect("serialize");
        match serde_json::from_slice::<BrokerCredentials>(&json).expect("deserialize") {
            BrokerCredentials::IbkrFix { host, port, sender_comp_id, target_comp_id, password, ssl } => {
                assert_eq!(host, "ibkr-gateway.example.com");
                assert_eq!(port, 4001);
                assert_eq!(sender_comp_id, "OMSIB");
                assert_eq!(target_comp_id, "IBKRTARGET");
                assert_eq!(password, "IBKRPASSWORD777");
                // The one that matters: an explicitly-stored false must NOT come back
                // as true via the serde default.
                assert!(!ssl, "explicit ssl:false must survive the round trip");
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let f = FeedCredentials::Databento { api_key: "db-key".into() };
        match serde_json::from_slice::<FeedCredentials>(&serde_json::to_vec(&f).expect("ser")).expect("de") {
            FeedCredentials::Databento { api_key } => assert_eq!(api_key, "db-key"),
        }
    }

    /// The other direction of the `ssl` default: a blob written before this field
    /// existed (or one that simply omits it) must come back `true`, not fail to
    /// parse or silently become `false`.
    #[test]
    fn ibkr_ssl_defaults_to_true_when_absent_from_the_blob() {
        let json = br#"{"kind":"IbkrFix","host":"h","port":4001,"sender_comp_id":"S","target_comp_id":"T","password":"p"}"#;
        match serde_json::from_slice::<BrokerCredentials>(json).expect("deserialize") {
            BrokerCredentials::IbkrFix { ssl, .. } => assert!(ssl, "missing ssl must default to true"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// THE regression that matters: the redacted view is what reaches an HTTP
    /// response, so no secret may appear in it. Non-secret fields must survive,
    /// otherwise the UI cannot show what is configured.
    #[test]
    fn redaction_drops_every_secret_and_keeps_the_rest() {
        let r = format!("{:?}", alpaca().redact());
        assert!(!r.contains("SUPERSECRETVALUE"), "alpaca secret leaked: {r}");

        let r = format!("{:?}", binance().redact());
        assert!(!r.contains("MIIBSECRET"), "private key leaked: {r}");
        assert!(!r.contains("BNKEY999"), "api key leaked: {r}");
        assert!(r.contains("fix-oe.testnet.binance.vision"), "host should be visible: {r}");
        assert!(r.contains("9000"), "port should be visible: {r}");
        assert!(r.contains("SPOT"), "target comp id should be visible: {r}");

        let r = format!("{:?}", ibkr().redact());
        assert!(!r.contains("IBKRPASSWORD777"), "ibkr password leaked: {r}");
        assert!(r.contains("ibkr-gateway.example.com"), "host should be visible: {r}");
        assert!(r.contains("4001"), "port should be visible: {r}");
        assert!(r.contains("OMSIB"), "sender comp id should be visible: {r}");
        assert!(r.contains("IBKRTARGET"), "target comp id should be visible: {r}");
        assert!(r.contains("false"), "ssl should be visible: {r}");

        let r = format!("{:?}", FeedCredentials::Databento { api_key: "db-key".into() }.redact());
        assert!(!r.contains("db-key"), "feed key leaked: {r}");
    }

    /// `{:?}` on the credentials themselves must not print secrets either — a
    /// panic message or a stray log line is how these escape.
    #[test]
    fn debug_redacts_secrets_on_the_credentials_themselves() {
        let r = format!("{:?}", alpaca());
        assert!(!r.contains("SUPERSECRETVALUE"), "leaked in Debug: {r}");
        let r = format!("{:?}", binance());
        assert!(!r.contains("MIIBSECRET"), "leaked in Debug: {r}");
        let r = format!("{:?}", ibkr());
        assert!(!r.contains("IBKRPASSWORD777"), "leaked in Debug: {r}");
    }

    /// An unknown tag is data written by a newer version, not garbage to guess at.
    #[test]
    fn unknown_variant_is_an_error_not_a_panic() {
        let r: Result<BrokerCredentials, _> = serde_json::from_slice(br#"{"kind":"Nasdaq"}"#);
        assert!(r.is_err());
    }

    /// A key too short to usefully mask must not come back whole — that would
    /// contradict the "not enough to use it" promise the redaction comment makes.
    #[test]
    fn tail4_masks_keys_too_short_to_redact_meaningfully() {
        assert_eq!(tail4(""), "");
        assert_eq!(tail4("abc"), "");
        assert_eq!(tail4("AKTESTKEY123"), "Y123");
    }
}
