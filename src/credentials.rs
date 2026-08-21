//! What a connection needs in order to authenticate, and what may be shown of it.
//!
//! Knows nothing about AES — sealing is `crate::secrets`' job. This module owns
//! the shapes, their JSON encoding, and the redacted view that is safe to put on
//! the wire.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::secrets::{self, MasterKey};

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

/// What is known about a connection's credentials.
#[derive(Debug)]
pub enum CredentialState<T> {
    Configured(T),
    /// The row exists but nothing has been stored — "needs setup".
    Unconfigured,
    /// Stored, but unusable. Never collapsed into `Unconfigured`: that would
    /// invite re-entry of a credential that is already there and hide a key
    /// problem behind what looks like a fresh install.
    Error(String),
}

#[derive(Debug)]
pub struct Connection<T> {
    pub code: String,
    /// `broker_code` for brokers, `provider` for feeds.
    pub kind: String,
    pub environment: Option<String>,
    pub status: String,
    pub credentials: CredentialState<T>,
    /// When `credentials` was last written — not the row's own `updated_at`,
    /// which also moves on non-credential edits (status, environment, ...).
    pub credentials_updated_at: Option<DateTime<Utc>>,
}

/// Decode is deliberately split from the SQL below: every failure case — no
/// blob, no key, wrong key, another connection's blob, unparseable payload —
/// is exercised in `mod tests` through the seal/open seam, with no database.
fn decode<T: serde::de::DeserializeOwned>(
    key: Option<&MasterKey>,
    code: &str,
    blob: Option<Vec<u8>>,
) -> CredentialState<T> {
    let Some(blob) = blob else { return CredentialState::Unconfigured };
    let Some(key) = key else {
        return CredentialState::Error(
            "credentials are stored but no master key is configured (set oms.master_key in oms.toml)".into(),
        );
    };
    match secrets::open(key, code, &blob) {
        Err(e) => CredentialState::Error(e.to_string()),
        Ok(plain) => match serde_json::from_slice(&plain) {
            Ok(v) => CredentialState::Configured(v),
            // Deliberately does not include the payload: it is a decrypted secret.
            Err(_) => CredentialState::Error("stored credentials are not in a recognised format".into()),
        },
    }
}

/// `decode` specialised to brokers — a thin wrapper so callers never have to
/// name the generic parameter.
pub fn decode_broker(
    key: Option<&MasterKey>,
    code: &str,
    blob: Option<Vec<u8>>,
) -> CredentialState<BrokerCredentials> {
    decode(key, code, blob)
}

/// `decode` specialised to feeds — see `decode_broker`.
pub fn decode_feed(
    key: Option<&MasterKey>,
    code: &str,
    blob: Option<Vec<u8>>,
) -> CredentialState<FeedCredentials> {
    decode(key, code, blob)
}

/// Every configured broker connection, credentials decoded under `key`. A
/// connection missing or unusable credentials is still returned — its state
/// says why, rather than being silently dropped from the list.
pub async fn load_brokers(
    pool: &PgPool,
    key: Option<&MasterKey>,
) -> Result<Vec<Connection<BrokerCredentials>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, String, String, Option<Vec<u8>>, Option<DateTime<Utc>>)>(
        "SELECT code, broker_code, environment, status, credentials, credentials_updated_at \
         FROM oms.broker_connection ORDER BY code",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(code, broker, env, status, blob, updated)| Connection {
            credentials: decode_broker(key, &code, blob),
            code,
            kind: broker,
            environment: Some(env),
            status,
            credentials_updated_at: updated,
        })
        .collect())
}

/// Every configured feed connection, credentials decoded under `key`. See
/// `load_brokers` — same shape, same reasoning.
pub async fn load_feeds(
    pool: &PgPool,
    key: Option<&MasterKey>,
) -> Result<Vec<Connection<FeedCredentials>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, String, String, Option<Vec<u8>>, Option<DateTime<Utc>>)>(
        "SELECT code, provider, status, credentials, credentials_updated_at \
         FROM oms.feed_connection ORDER BY code",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(code, provider, status, blob, updated)| Connection {
            credentials: decode_feed(key, &code, blob),
            code,
            kind: provider,
            environment: None,
            status,
            credentials_updated_at: updated,
        })
        .collect())
}

/// Seals `c` and writes it onto an existing row. Deliberately an `UPDATE`, not
/// an upsert: broker rows are created at boot by
/// `bootstrap::ensure_broker_connections` and are FK targets for `account`,
/// `order_state` and `recon_run`, so inventing one here from a credential save
/// would be wrong — the row must already exist. Returns `RowNotFound` if
/// `code` does not match any row: an `UPDATE` that matches nothing still
/// returns `Ok` from sqlx, so without this check a typo'd code would report
/// success while storing nothing.
pub async fn save_broker(
    pool: &PgPool,
    key: &MasterKey,
    code: &str,
    c: &BrokerCredentials,
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_vec(c).expect("credentials always serialize");
    let sealed = secrets::seal(key, code, &json);
    let result = sqlx::query(
        "UPDATE oms.broker_connection \
         SET credentials = $2, credentials_updated_at = now(), updated_at = now() \
         WHERE code = $1",
    )
    .bind(code)
    .bind(&sealed)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Seals `c` and upserts it by `code`, the primary key. Unlike
/// `save_broker`, a feed connection has no separate bootstrap step that
/// creates the row first, so this call is the thing that creates it.
pub async fn save_feed(pool: &PgPool, key: &MasterKey, code: &str, c: &FeedCredentials) -> Result<(), sqlx::Error> {
    let json = serde_json::to_vec(c).expect("credentials always serialize");
    let sealed = secrets::seal(key, code, &json);
    // Upserts on `code`, the primary key — feed_connection has no unique
    // constraint on (provider, dataset), so this is the only conflict target
    // that behaves.
    sqlx::query(
        "INSERT INTO oms.feed_connection (code, provider, credentials, credentials_updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (code) DO UPDATE \
           SET provider = EXCLUDED.provider, \
               credentials = EXCLUDED.credentials, \
               credentials_updated_at = now(), updated_at = now()",
    )
    .bind(code)
    .bind(match c { FeedCredentials::Databento { .. } => "DATABENTO" })
    .bind(&sealed)
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether anything is stored at all. `serve()` uses this to decide whether a
/// missing master key is fatal. NOTE: on a database where migration 0021 has
/// not been applied, this fails with `42P01 undefined_table` rather than
/// returning `Ok(false)` — a caller must not fold that `Err` into `false`,
/// since doing so would make a missing master key stop being fatal at exactly
/// the moment the schema itself is broken.
pub async fn any_credentials_stored(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM oms.broker_connection WHERE credentials IS NOT NULL) \
              + (SELECT count(*) FROM oms.feed_connection   WHERE credentials IS NOT NULL)",
    )
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{parse_master_key, seal};

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

    fn key() -> crate::secrets::MasterKey {
        parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("key")
    }

    /// A row whose blob decrypts becomes Configured.
    #[test]
    fn a_good_blob_decodes_to_configured() {
        let json = serde_json::to_vec(&alpaca()).expect("ser");
        let sealed = seal(&key(), "alpaca-paper", &json);
        match decode_broker(Some(&key()), "alpaca-paper", Some(sealed)) {
            CredentialState::Configured(BrokerCredentials::Alpaca { key: k, .. }) => {
                assert_eq!(k, "AKTESTKEY123");
            }
            other => panic!("expected Configured, got {other:?}"),
        }
    }

    /// A null blob is a connection that exists but has never been configured.
    #[test]
    fn a_null_blob_is_unconfigured() {
        assert!(matches!(
            decode_broker(Some(&key()), "alpaca-paper", None),
            CredentialState::Unconfigured
        ));
    }

    /// The wrong key must be reported, never silently downgraded to Unconfigured —
    /// that would invite the operator to re-enter a credential that is already
    /// there and mask a key-management problem.
    #[test]
    fn an_undecryptable_blob_is_an_error_not_unconfigured() {
        let other = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("key");
        let sealed = seal(&other, "alpaca-paper", b"{}");
        assert!(matches!(
            decode_broker(Some(&key()), "alpaca-paper", Some(sealed)),
            CredentialState::Error(_)
        ));
    }

    /// A blob sealed for a different connection must not decode here.
    #[test]
    fn a_blob_from_another_connection_is_an_error() {
        let json = serde_json::to_vec(&alpaca()).expect("ser");
        let sealed = seal(&key(), "alpaca-live", &json);
        assert!(matches!(
            decode_broker(Some(&key()), "alpaca-paper", Some(sealed)),
            CredentialState::Error(_)
        ));
    }

    /// Credentials present but no key configured is a distinct, nameable problem.
    #[test]
    fn stored_credentials_with_no_key_is_an_error() {
        assert!(matches!(decode_broker(None, "alpaca-paper", Some(vec![0u8; 40])), CredentialState::Error(_)));
    }

    /// Decryptable but not parseable — a shape written by a newer version.
    #[test]
    fn undecodable_json_is_an_error() {
        let sealed = seal(&key(), "alpaca-paper", b"not json at all");
        assert!(matches!(
            decode_broker(Some(&key()), "alpaca-paper", Some(sealed)),
            CredentialState::Error(_)
        ));
    }

    /// The error string is shown in the cockpit and logged — it must not carry
    /// any part of the ciphertext or the key.
    #[test]
    fn error_strings_carry_no_secret_material() {
        let sealed = seal(&key(), "alpaca-paper", b"not json at all");
        // The plaintext here is "not json at all"; the message must not quote it
        // and must not hex-dump the ciphertext. (Not asserting on key material
        // here: the error string is a fixed literal that never embeds the key
        // in any case, so a `contains(<base64 of the key>)` check can never
        // fail and would only assert that fact about `key()`, not about this
        // code path.)
        if let CredentialState::Error(msg) = decode_broker(Some(&key()), "alpaca-paper", Some(sealed)) {
            assert!(!msg.contains("not json at all"), "decrypted payload leaked: {msg}");
            assert!(msg.len() < 120, "suspiciously long, likely dumping data: {msg}");
        } else {
            panic!("expected an error");
        }
    }

    /// `decode_feed` shares `decode`'s implementation with `decode_broker`, but
    /// is pinned separately so its AAD handling — sealed and opened under the
    /// feed's own `code` — is exercised directly rather than only by proxy.
    #[test]
    fn a_good_feed_blob_decodes_to_configured() {
        let creds = FeedCredentials::Databento { api_key: "db-key-123".into() };
        let json = serde_json::to_vec(&creds).expect("ser");
        let sealed = seal(&key(), "databento-opra", &json);
        match decode_feed(Some(&key()), "databento-opra", Some(sealed)) {
            CredentialState::Configured(FeedCredentials::Databento { api_key }) => {
                assert_eq!(api_key, "db-key-123");
            }
            other => panic!("expected Configured, got {other:?}"),
        }
    }
}
