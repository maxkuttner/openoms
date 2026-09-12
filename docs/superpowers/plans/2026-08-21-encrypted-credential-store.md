# Encrypted Credential Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move broker and market-data credentials out of the environment into an encrypted store in Postgres, and register adapters at boot from that store instead of from `env::var`.

**Architecture:** A new `src/secrets.rs` seals and opens byte strings with AES-256-GCM under the master key already written to `oms.toml` by `oms init`. Credentials are `serde` enums sealed as JSON into a single `BYTEA` column on `oms.broker_connection` and a new `oms.feed_connection`. A new `src/credentials.rs` owns loading, decrypting and redacting them. `serve()` builds its adapters from that module. `oms config import-env` migrates an existing `.env` in one shot; `oms config rotate-key` re-wraps every row.

**Plan 2 of 4** for this spec. Plan 1 (bootstrap `oms.toml`) is complete and merged into this branch's history; this plan consumes the `master_key` it generates.

**Tech Stack:** Rust 2021, sqlx 0.8 (Postgres), serde 1, `aes-gcm` 0.10 (new), `rand` 0.8, `base64` 0.22, clap 4, tokio.

**Spec:** `docs/superpowers/specs/2026-08-20-connection-config-gui-design.md`

## Global Constraints

- **Rust edition 2021.** Doc comments explain *why*, not *what*.
- **AES-256-GCM.** Stored bytes are `[12-byte nonce][ciphertext‖tag]`, nonce fresh per write from the OS RNG.
- **The connection's `code` is the AAD**, binding each blob to its row so a copied `credentials` value cannot decrypt onto a different connection.
- **Because the code is the AAD, renaming a connection orphans its credentials.** Any path that can change a `code` must decrypt under the old code and re-seal under the new one in the same transaction, or must refuse the rename. Nothing in this plan renames a connection; Plan 4's editing UI is where this bites, and it is called out in the spec's follow-ups.
- **Secrets never reach a log, a `Debug`, an error message, or an HTTP response.** Every credential type gets a hand-written redacting `Debug`, matching the existing impls in `src/config.rs` and `src/setup/init.rs`.
- **No plaintext credential is ever written to disk** — not to `oms.toml`, not to a temp file.
- **After this plan, the environment is no longer consulted for broker or feed credentials.** No fallback, no precedence: the database is the only source. `import-env` is the one-shot bridge.
- **A missing master key with credential rows present is fatal at boot.** Starting "successfully" with no brokers would be a lie.
- **An undecryptable credential is reported, never silently treated as unconfigured** — that invites re-entry and masks a key-management problem.
- **`oms database init|migrate|drop|status` keep their current behaviour and flags.** CI drives them directly.
- **Tests needing Postgres are `#[ignore]`d**, matching `migrate.rs::applying_twice_is_a_no_op`.

---

## File Structure

**Create:**
- `src/secrets.rs` — the crypto primitive and the master key type. `seal`, `open`, `MasterKey`. One responsibility: bytes in, sealed bytes out, and back.
- `src/credentials.rs` — the domain layer: the credential enums, their redaction, and the load/save/list operations against the two tables.
- `db/migrations/ods/oms/0021_CREATE_CREDENTIAL_STORAGE.sql` — the columns and the new table.

**Modify:**
- `Cargo.toml` — add `aes-gcm`.
- `src/config.rs` — expose the master key with an `OMS_MASTER_KEY` env override.
- `src/main.rs` — register `mod secrets; mod credentials;`, build adapters from the store, add `Command::Config`.
- `src/fix/mod.rs` — `start_ibkr` / `start_binance` take credentials as parameters instead of reading env.
- `src/setup/database/assets.rs` — bump the `oms` migration count 20 → 21.

**Why two modules rather than one:** `secrets.rs` knows nothing about brokers and `credentials.rs` knows nothing about AES. That seam is what lets the crypto be tested exhaustively with byte strings, and the domain layer be tested with a stub key.

---

### Task 1: The crypto primitive

**Files:**
- Create: `src/secrets.rs`
- Modify: `Cargo.toml`, `src/main.rs` (add `mod secrets;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct MasterKey([u8; 32])` — with a `Debug` that prints `MasterKey(<redacted>)`
  - `pub fn parse_master_key(s: &str) -> Result<MasterKey, SecretError>` — accepts the `base64:` prefix `oms init` writes
  - `pub fn seal(key: &MasterKey, aad: &str, plaintext: &[u8]) -> Vec<u8>`
  - `pub fn open(key: &MasterKey, aad: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError>`
  - `pub enum SecretError { BadKey(String), Decrypt, Malformed }`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, beside `base64` and `rand`:

```toml
aes-gcm = "0.10"
```

- [ ] **Step 2: Write the failing tests**

Create `src/secrets.rs` with only the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("key")
    }

    #[test]
    fn round_trips() {
        let sealed = seal(&key(), "alpaca-paper", b"hello");
        assert_eq!(open(&key(), "alpaca-paper", &sealed).expect("open"), b"hello");
    }

    /// Two seals of the same plaintext must differ — a fresh nonce per write is
    /// what stops an observer learning that two connections share a credential.
    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        assert_ne!(seal(&key(), "a", b"same"), seal(&key(), "a", b"same"));
    }

    /// The AAD binds a blob to its row. Copying `credentials` from one connection
    /// onto another must fail to decrypt rather than silently authenticating as
    /// the wrong account.
    #[test]
    fn wrong_aad_fails_to_open() {
        let sealed = seal(&key(), "alpaca-paper", b"hello");
        assert!(matches!(open(&key(), "alpaca-live", &sealed), Err(SecretError::Decrypt)));
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let other = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("key");
        let sealed = seal(&key(), "a", b"hello");
        assert!(matches!(open(&other, "a", &sealed), Err(SecretError::Decrypt)));
    }

    /// A flipped bit anywhere — nonce, ciphertext or tag — must be rejected, not
    /// silently produce garbage plaintext.
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut sealed = seal(&key(), "a", b"hello");
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(matches!(open(&key(), "a", &sealed), Err(SecretError::Decrypt)));

        let mut sealed = seal(&key(), "a", b"hello");
        sealed[0] ^= 0x01; // nonce byte
        assert!(matches!(open(&key(), "a", &sealed), Err(SecretError::Decrypt)));
    }

    /// Anything shorter than a nonce plus a tag cannot be a sealed value.
    #[test]
    fn truncated_input_is_malformed_not_a_panic() {
        assert!(matches!(open(&key(), "a", &[0u8; 4]), Err(SecretError::Malformed)));
        assert!(matches!(open(&key(), "a", &[]), Err(SecretError::Malformed)));
    }

    #[test]
    fn parses_the_key_written_by_oms_init() {
        assert!(parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").is_ok());
        // Wrong length, not base64, missing prefix.
        assert!(matches!(parse_master_key("base64:AAAA"), Err(SecretError::BadKey(_))));
        assert!(matches!(parse_master_key("base64:!!!!"), Err(SecretError::BadKey(_))));
        assert!(matches!(parse_master_key("hunter2"), Err(SecretError::BadKey(_))));
    }

    /// The key must never print itself, in any formatting context.
    #[test]
    fn debug_redacts_the_key() {
        let rendered = format!("{:?}", key());
        assert!(!rendered.contains("AAAA"), "key leaked into {rendered}");
        assert!(rendered.contains("redacted"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test secrets::tests`
Expected: FAIL to compile — nothing in `super` exists yet.

- [ ] **Step 4: Write the implementation**

Above the tests in `src/secrets.rs`:

```rust
//! Sealing and opening secrets with AES-256-GCM.
//!
//! Knows nothing about brokers or connections — bytes in, sealed bytes out — so
//! the crypto can be tested exhaustively without a database and the domain layer
//! can be tested without real keys.
//!
//! The stored form is `[12-byte nonce][ciphertext‖tag]`. The nonce is fresh per
//! write: reusing one under the same key is the failure mode that breaks GCM
//! outright, so it is generated from the OS RNG every time rather than counted.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;

const NONCE_LEN: usize = 12;
/// GCM's authentication tag. Present at the end of every ciphertext.
const TAG_LEN: usize = 16;

/// The key everything is sealed under. Lives in `oms.toml`, never in the database.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

// Hand-written: a `{:?}` in a log line or a panic message must never print the
// one value that decrypts every stored credential.
impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

#[derive(Debug, PartialEq)]
pub enum SecretError {
    /// The configured key is not 32 bytes of base64.
    BadKey(String),
    /// Authentication failed: wrong key, wrong AAD, or tampered bytes. These are
    /// deliberately indistinguishable — telling them apart tells an attacker
    /// which half they guessed right.
    Decrypt,
    /// Too short to contain a nonce and a tag.
    Malformed,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::BadKey(m) => write!(f, "invalid master key: {m}"),
            SecretError::Decrypt => f.write_str("could not decrypt (wrong key, wrong connection, or corrupted data)"),
            SecretError::Malformed => f.write_str("stored value is too short to be a sealed secret"),
        }
    }
}

impl std::error::Error for SecretError {}

/// Parse the `base64:`-prefixed form `oms init` writes into `oms.toml`.
pub fn parse_master_key(s: &str) -> Result<MasterKey, SecretError> {
    let body = s
        .strip_prefix("base64:")
        .ok_or_else(|| SecretError::BadKey("expected a \"base64:\" prefix".into()))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| SecretError::BadKey("not valid base64".into()))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| SecretError::BadKey("must decode to exactly 32 bytes".into()))?;
    Ok(MasterKey(bytes))
}

pub fn seal(key: &MasterKey, aad: &str, plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("32-byte key is the right length");
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encryption with a correct key and nonce length has no failure mode worth
    // propagating — the only documented error is a plaintext larger than GCM's
    // ~64GiB limit, which a credential blob cannot reach.
    let ciphertext = cipher
        .encrypt(nonce, Payload { msg: plaintext, aad: aad.as_bytes() })
        .expect("AES-GCM encryption cannot fail for a credential-sized payload");

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

pub fn open(key: &MasterKey, aad: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(SecretError::Malformed);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("32-byte key is the right length");
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ciphertext, aad: aad.as_bytes() })
        .map_err(|_| SecretError::Decrypt)
}
```

Add `mod secrets;` to `src/main.rs` beside the other module declarations.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test secrets::tests`
Expected: 8 passed.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/secrets.rs src/main.rs
git commit -m "feat(secrets): AES-256-GCM sealing under the oms.toml master key

The connection code is the AAD, so a credentials blob copied onto another
row fails to decrypt rather than silently authenticating as the wrong
account. Wrong key, wrong AAD and tampered bytes are one error on
purpose — distinguishing them tells an attacker which half they guessed."
```

---

### Task 2: The master key, resolved

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: `crate::secrets::{MasterKey, parse_master_key, SecretError}` (Task 1); `FileConfig` (existing).
- Produces: `pub fn master_key(file: Option<&FileConfig>) -> Option<Result<MasterKey, SecretError>>` — `None` means none configured anywhere.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/config.rs`:

```rust
    /// The key follows the same env-over-file precedence as everything else, so a
    /// container can inject it without shipping a config file.
    #[test]
    fn master_key_prefers_env_then_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("OMS_MASTER_KEY");

        let file = parse(
            "[oms]\nmaster_key = \"base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"\n",
        )
        .expect("parse");
        assert!(master_key(Some(&file)).expect("configured").is_ok());

        std::env::set_var("OMS_MASTER_KEY", "base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=");
        assert!(master_key(Some(&file)).expect("configured").is_ok());
        std::env::remove_var("OMS_MASTER_KEY");
    }

    /// Nothing configured is not an error — an install with no credentials yet is
    /// perfectly valid. The caller decides whether that is fatal.
    #[test]
    fn master_key_absent_is_none_not_an_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("OMS_MASTER_KEY");
        assert!(master_key(None).is_none());
        assert!(master_key(Some(&FileConfig::default())).is_none());
    }

    /// A configured but unusable key must surface as an error, never as "absent" —
    /// silently treating a typo'd key as "no credentials" would look like data loss.
    #[test]
    fn master_key_present_but_invalid_is_an_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("OMS_MASTER_KEY");
        let file = parse("[oms]\nmaster_key = \"base64:AAAA\"\n").expect("parse");
        assert!(master_key(Some(&file)).expect("configured").is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test config::tests::master_key`
Expected: FAIL to compile — `master_key` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/config.rs`:

```rust
/// The master key, on the usual env-over-file tiers.
///
/// Three-way return on purpose. `None` means no key is configured, which is
/// normal for an install that has never stored a credential. `Some(Err(_))` means
/// one was configured and is unusable — a typo must never be mistaken for
/// "absent", because absent is survivable and a wrong key is not.
pub fn master_key(
    file: Option<&FileConfig>,
) -> Option<Result<crate::secrets::MasterKey, crate::secrets::SecretError>> {
    std::env::var("OMS_MASTER_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| file.and_then(|f| f.oms.master_key.clone()))
        .filter(|v| !v.is_empty())
        .map(|raw| crate::secrets::parse_master_key(&raw))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test config::tests`
Expected: all pass, including the three new ones.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): resolve the master key from env or oms.toml

Three-way return: absent is survivable and normal for an install with no
stored credentials; configured-but-invalid is not, and must never be
mistaken for absent."
```

---

### Task 3: Storage schema

**Files:**
- Create: `db/migrations/ods/oms/0021_CREATE_CREDENTIAL_STORAGE.sql`
- Modify: `src/setup/database/assets.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the `credentials` columns on `oms.broker_connection`, and the `oms.feed_connection` table.

- [ ] **Step 1: Write the migration**

Read two neighbours in `db/migrations/ods/oms/` first and match their style. Create `db/migrations/ods/oms/0021_CREATE_CREDENTIAL_STORAGE.sql`:

```sql
-- Credentials move out of the environment and into the database, sealed with the
-- master key from oms.toml. The database never sees plaintext, and a dump without
-- that key yields nothing usable.
--
-- One opaque blob per connection rather than a column per field: the shapes differ
-- per broker (key/secret for a REST API, host/port/comp-ids/private-key for a FIX
-- session), and serde already models that. The fields worth querying — broker,
-- environment, status — are already columns here.
--
-- Nullable on purpose: a connection may exist unconfigured, which is exactly what
-- the cockpit shows as "needs setup".

ALTER TABLE broker_connection
    ADD COLUMN credentials            BYTEA,
    ADD COLUMN credentials_updated_at TIMESTAMPTZ,
    ADD COLUMN credentials_updated_by TEXT;   -- reserved; null until user accounts exist

COMMENT ON COLUMN broker_connection.credentials IS
    'AES-256-GCM sealed JSON: [12-byte nonce][ciphertext||tag]. AAD is this row''s code.';

-- Feeds are not brokers — a market-data provider has no orders, no accounts and no
-- routing — so they get their own table rather than a nullable-everything column
-- set on broker_connection.
CREATE TABLE feed_connection (
    code                   TEXT PRIMARY KEY,          -- 'databento-opra'
    provider               TEXT NOT NULL,             -- DATABENTO
    dataset                TEXT,                      -- OPRA.PILLAR
    status                 TEXT NOT NULL DEFAULT 'ACTIVE',
    credentials            BYTEA,
    credentials_updated_at TIMESTAMPTZ,
    credentials_updated_by TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE','DISABLED'))
);

COMMENT ON TABLE feed_connection IS
    'Configured market-data providers; credentials sealed with the oms.toml master key.';
```

- [ ] **Step 2: Watch the pinned count fail**

Run: `cargo test setup::database::assets`
Expected: FAIL — `embeds_every_migration` asserts 20 `oms` migrations, now 21.

- [ ] **Step 3: Update the pinned count**

In `src/setup/database/assets.rs`, change the `oms` target's assertion from `20` to `21`. Leave the `public` target's `24` alone.

Run: `cargo test setup::database::assets`
Expected: PASS.

- [ ] **Step 4: Apply it against a throwaway Postgres**

```bash
docker run -d --name oms-t3 -e POSTGRES_PASSWORD=secret -p 55435:5432 postgres:16
sleep 4
POSTGRES_HOST=localhost POSTGRES_PORT=55435 POSTGRES_USERNAME=postgres \
POSTGRES_PASSWORD=secret POSTGRES_DATABASE=ods cargo run -q -- database init
docker exec oms-t3 psql -U postgres -d ods -c '\d oms.feed_connection'
docker exec oms-t3 psql -U postgres -d ods -c '\d oms.broker_connection'
docker rm -f oms-t3
```

Expected: `feed_connection` exists; `broker_connection` shows the three new columns.

- [ ] **Step 5: Commit**

```bash
git add db/migrations/ods/oms/0021_CREATE_CREDENTIAL_STORAGE.sql src/setup/database/assets.rs
git commit -m "feat(database): storage for sealed connection credentials

One opaque blob per connection rather than a column per field — the shapes
differ per broker and serde already models that. The fields worth querying
are already columns. Feeds get their own table: a data provider has no
orders, accounts or routing."
```

---

### Task 4: Credential types and redaction

**Files:**
- Create: `src/credentials.rs`
- Modify: `src/main.rs` (add `mod credentials;`)

**Interfaces:**
- Consumes: `crate::secrets` (Task 1).
- Produces:
  - `pub enum BrokerCredentials { Alpaca { key, secret }, IbkrFix { host, port, sender_comp_id, target_comp_id, password, ssl }, BinanceFix { host, port, sender_comp_id, target_comp_id, api_key, private_key } }`
  - `pub enum FeedCredentials { Databento { api_key: String } }`
  - `pub struct Redacted { pub fields: Vec<(String, Option<String>)> }`
  - `pub trait Redact { fn redact(&self) -> Redacted; }` implemented for both enums

- [ ] **Step 1: Write the failing tests**

Create `src/credentials.rs` with only the tests:

```rust
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

        let f = FeedCredentials::Databento { api_key: "db-key".into() };
        match serde_json::from_slice::<FeedCredentials>(&serde_json::to_vec(&f).expect("ser")).expect("de") {
            FeedCredentials::Databento { api_key } => assert_eq!(api_key, "db-key"),
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
    }

    /// An unknown tag is data written by a newer version, not garbage to guess at.
    #[test]
    fn unknown_variant_is_an_error_not_a_panic() {
        let r: Result<BrokerCredentials, _> = serde_json::from_slice(br#"{"kind":"Nasdaq"}"#);
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test credentials::tests`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Above the tests in `src/credentials.rs`:

```rust
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

fn tail4(s: &str) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(4)).collect()
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
```

Add `mod credentials;` to `src/main.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test credentials::tests`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/credentials.rs src/main.rs
git commit -m "feat(credentials): credential shapes and their redacted view

Hand-written Debug on both enums — deriving it would print every secret
the first time one reached a panic message. The redacted view is what
goes on the wire: secrets named but valueless, everything else visible so
the UI can show what is configured.

Binance's private key is PEM contents, not a path: a path is what stops a
credential being configurable from anywhere but the server's shell."
```

---

### Task 5: The store

**Files:**
- Modify: `src/credentials.rs`

**Interfaces:**
- Consumes: `crate::secrets::{MasterKey, seal, open, SecretError}`; the schema from Task 3.
- Produces:
  - `pub struct Connection<T> { pub code: String, pub kind: String, pub environment: Option<String>, pub status: String, pub credentials: CredentialState<T>, pub updated_at: Option<DateTime<Utc>> }`
  - `pub enum CredentialState<T> { Configured(T), Unconfigured, Error(String) }`
  - `pub async fn load_brokers(pool: &PgPool, key: Option<&MasterKey>) -> Result<Vec<Connection<BrokerCredentials>>, sqlx::Error>`
  - `pub async fn load_feeds(pool: &PgPool, key: Option<&MasterKey>) -> Result<Vec<Connection<FeedCredentials>>, sqlx::Error>`
  - `pub async fn save_broker(pool: &PgPool, key: &MasterKey, code: &str, c: &BrokerCredentials) -> Result<(), sqlx::Error>`
  - `pub async fn save_feed(pool: &PgPool, key: &MasterKey, code: &str, c: &FeedCredentials) -> Result<(), sqlx::Error>`
  - `pub async fn any_credentials_stored(pool: &PgPool) -> Result<bool, sqlx::Error>`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/credentials.rs`. These test the decode path without a database by exercising the seal/open seam directly:

```rust
    use crate::secrets::{parse_master_key, seal};

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
        assert!(matches!(
            decode_broker(None, "alpaca-paper", Some(vec![0u8; 40])),
            CredentialState::Error(_)
        ));
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
        // The plaintext here is "not json at all"; the message must not quote it,
        // must not carry the key, and must not hex-dump the ciphertext.
        if let CredentialState::Error(msg) = decode_broker(Some(&key()), "alpaca-paper", Some(sealed)) {
            assert!(!msg.contains("not json at all"), "decrypted payload leaked: {msg}");
            assert!(!msg.contains("AAAA"), "key material leaked: {msg}");
            assert!(msg.len() < 120, "suspiciously long, likely dumping data: {msg}");
        } else {
            panic!("expected an error");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test credentials::tests`
Expected: FAIL to compile — `decode_broker`, `CredentialState` not found.

- [ ] **Step 3: Write the implementation**

Add to `src/credentials.rs`. Note `decode_broker`/`decode_feed` are separated from the SQL precisely so the seven cases above are testable without Postgres:

```rust
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::secrets::{self, MasterKey};

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
    pub updated_at: Option<DateTime<Utc>>,
}

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

pub fn decode_broker(
    key: Option<&MasterKey>,
    code: &str,
    blob: Option<Vec<u8>>,
) -> CredentialState<BrokerCredentials> {
    decode(key, code, blob)
}

pub fn decode_feed(
    key: Option<&MasterKey>,
    code: &str,
    blob: Option<Vec<u8>>,
) -> CredentialState<FeedCredentials> {
    decode(key, code, blob)
}

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
            updated_at: updated,
        })
        .collect())
}

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
            updated_at: updated,
        })
        .collect())
}

pub async fn save_broker(
    pool: &PgPool,
    key: &MasterKey,
    code: &str,
    c: &BrokerCredentials,
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_vec(c).expect("credentials always serialize");
    let sealed = secrets::seal(key, code, &json);
    sqlx::query(
        "UPDATE oms.broker_connection \
         SET credentials = $2, credentials_updated_at = now(), updated_at = now() \
         WHERE code = $1",
    )
    .bind(code)
    .bind(&sealed)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn save_feed(
    pool: &PgPool,
    key: &MasterKey,
    code: &str,
    c: &FeedCredentials,
) -> Result<(), sqlx::Error> {
    let json = serde_json::to_vec(c).expect("credentials always serialize");
    let sealed = secrets::seal(key, code, &json);
    sqlx::query(
        "INSERT INTO oms.feed_connection (code, provider, credentials, credentials_updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (code) DO UPDATE \
           SET credentials = EXCLUDED.credentials, \
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
/// missing master key is fatal.
pub async fn any_credentials_stored(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM oms.broker_connection WHERE credentials IS NOT NULL) \
              + (SELECT count(*) FROM oms.feed_connection   WHERE credentials IS NOT NULL)",
    )
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test credentials::tests`
Expected: 12 passed.

- [ ] **Step 5: Commit**

```bash
git add src/credentials.rs
git commit -m "feat(credentials): load, save and decode sealed credentials

decode() is split from the SQL so every failure case — no key, wrong key,
another connection's blob, unparseable payload — is testable without a
database. An undecryptable blob reports an error rather than posing as
Unconfigured: the second invites re-entry and hides a key problem."
```

---

### Task 6: `oms config import-env`

**Files:**
- Create: `src/setup/import_env.rs`
- Modify: `src/setup/mod.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `credentials::{BrokerCredentials, FeedCredentials, save_broker, save_feed}`; `config::master_key`.
- Produces:
  - `pub fn scan_env() -> Vec<(String, BrokerCredentials)>` and `pub fn scan_feed_env() -> Vec<(String, FeedCredentials)>`
  - `pub async fn run(pool: &PgPool, key: &MasterKey) -> Result<usize, Box<dyn std::error::Error>>`

- [ ] **Step 1: Write the failing tests**

Create `src/setup/import_env.rs` with only the tests. Use the `ENV_LOCK` pattern from `src/setup/database/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        for k in [
            "ALPACA_ENV", "ALPACA_PAPER_API_KEY", "ALPACA_PAPER_API_SECRET",
            "ALPACA_LIVE_API_KEY", "ALPACA_LIVE_API_SECRET",
            "BINANCE_PAPER_FIX_HOST", "BINANCE_PAPER_API_KEY", "BINANCE_PAPER_PRIVATE_KEY_PATH",
            "IBKR_PAPER_FIX_HOST", "IBKR_PAPER_FIX_PASSWORD",
            "DATABENTO_API_KEY",
        ] {
            std::env::remove_var(k);
        }
    }

    /// Nothing set imports nothing — running this on a clean machine must not
    /// invent empty credentials.
    #[test]
    fn an_empty_environment_yields_nothing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        assert!(scan_env().is_empty());
        assert!(scan_feed_env().is_empty());
        clear();
    }

    #[test]
    fn finds_an_alpaca_pair_under_its_connection_code() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "AKTEST");
        std::env::set_var("ALPACA_PAPER_API_SECRET", "SECRET");
        let found = scan_env();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "alpaca-paper", "must match broker_connection.code");
        clear();
    }

    /// A half-configured broker is a mistake, not a credential — importing it
    /// would store something that cannot authenticate.
    #[test]
    fn a_half_configured_broker_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "AKTEST"); // no secret
        assert!(scan_env().is_empty());
        clear();
    }

    #[test]
    fn finds_both_alpaca_environments() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "P");
        std::env::set_var("ALPACA_PAPER_API_SECRET", "PS");
        std::env::set_var("ALPACA_LIVE_API_KEY", "L");
        std::env::set_var("ALPACA_LIVE_API_SECRET", "LS");
        let codes: Vec<_> = scan_env().into_iter().map(|(c, _)| c).collect();
        assert!(codes.contains(&"alpaca-paper".to_string()));
        assert!(codes.contains(&"alpaca-live".to_string()));
        clear();
    }

    #[test]
    fn finds_databento() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("DATABENTO_API_KEY", "db-xxx");
        let found = scan_feed_env();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "databento-opra");
        clear();
    }

    /// Binance's private key lives at a path in the environment and as PEM bytes
    /// in the store — an unreadable path must be skipped loudly, not stored empty.
    #[test]
    fn binance_with_an_unreadable_key_path_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("BINANCE_PAPER_FIX_HOST", "fix.example.com");
        std::env::set_var("BINANCE_PAPER_API_KEY", "K");
        std::env::set_var("BINANCE_PAPER_PRIVATE_KEY_PATH", "/nonexistent/nope.pem");
        assert!(scan_env().is_empty());
        clear();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test setup::import_env`
Expected: FAIL to compile.

- [ ] **Step 3: Write the implementation**

Above the tests in `src/setup/import_env.rs`:

```rust
//! `oms config import-env` — the one-shot bridge off environment credentials.
//!
//! Reads the `{BROKER}_{ENV}_*` variables the OMS used to consult at boot, seals
//! them into the store, and reports what it took. Run once; after that the
//! environment is no longer consulted for credentials at all.
//!
//! Deliberately skips anything half-configured. A key with no secret is a typo,
//! and importing it would store a credential that cannot authenticate — which
//! then looks like a broker outage rather than a missing value.

use sqlx::PgPool;

use crate::credentials::{save_broker, save_feed, BrokerCredentials, FeedCredentials};
use crate::secrets::MasterKey;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Broker credentials found in the environment, keyed by `broker_connection.code`.
pub fn scan_env() -> Vec<(String, BrokerCredentials)> {
    let mut out = Vec::new();

    for env_name in ["PAPER", "LIVE"] {
        if let (Some(key), Some(secret)) = (
            var(&format!("ALPACA_{env_name}_API_KEY")),
            var(&format!("ALPACA_{env_name}_API_SECRET")),
        ) {
            out.push((
                format!("alpaca-{}", env_name.to_lowercase()),
                BrokerCredentials::Alpaca { key, secret },
            ));
        }

        let p = format!("IBKR_{env_name}");
        if let Some(host) = var(&format!("{p}_FIX_HOST")) {
            out.push((
                format!("ibkr-{}", env_name.to_lowercase()),
                BrokerCredentials::IbkrFix {
                    host,
                    port: var(&format!("{p}_FIX_PORT")).and_then(|s| s.parse().ok()).unwrap_or(4001),
                    sender_comp_id: var(&format!("{p}_SENDER_COMP_ID")).unwrap_or_else(|| "OMS".into()),
                    target_comp_id: var(&format!("{p}_TARGET_COMP_ID")).unwrap_or_else(|| "IBKR".into()),
                    password: var(&format!("{p}_FIX_PASSWORD")).unwrap_or_default(),
                    ssl: var(&format!("{p}_FIX_SSL")).map(|s| s != "N").unwrap_or(true),
                },
            ));
        }

        let p = format!("BINANCE_{env_name}");
        if let (Some(host), Some(api_key), Some(path)) = (
            var(&format!("{p}_FIX_HOST")),
            var(&format!("{p}_API_KEY")),
            var(&format!("{p}_PRIVATE_KEY_PATH")),
        ) {
            // The store holds PEM bytes, not a path, so the file has to be read
            // now — while the operator is present to fix it if it is missing.
            match std::fs::read_to_string(&path) {
                Ok(private_key) => out.push((
                    format!("binance-{}", env_name.to_lowercase()),
                    BrokerCredentials::BinanceFix {
                        host,
                        port: var(&format!("{p}_FIX_PORT")).and_then(|s| s.parse().ok()).unwrap_or(9000),
                        sender_comp_id: var(&format!("{p}_SENDER_COMP_ID")).unwrap_or_else(|| "OMS".into()),
                        target_comp_id: var(&format!("{p}_TARGET_COMP_ID")).unwrap_or_else(|| "SPOT".into()),
                        api_key,
                        private_key,
                    },
                )),
                Err(e) => eprintln!("  skipped binance-{}: cannot read {path}: {e}", env_name.to_lowercase()),
            }
        }
    }

    out
}

/// Feed credentials found in the environment, keyed by `feed_connection.code`.
pub fn scan_feed_env() -> Vec<(String, FeedCredentials)> {
    match var("DATABENTO_API_KEY") {
        Some(api_key) => vec![("databento-opra".to_string(), FeedCredentials::Databento { api_key })],
        None => Vec::new(),
    }
}

/// Import everything found. Returns how many credentials were stored.
pub async fn run(pool: &PgPool, key: &MasterKey) -> Result<usize, Box<dyn std::error::Error>> {
    let brokers = scan_env();
    let feeds = scan_feed_env();
    if brokers.is_empty() && feeds.is_empty() {
        println!("nothing to import — no broker or feed credentials found in the environment");
        return Ok(0);
    }

    let mut n = 0;
    for (code, cred) in &brokers {
        // The connection row must exist first: credentials attach to a configured
        // routing target, they do not create one.
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM oms.broker_connection WHERE code = $1")
                .bind(code)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            println!("  skipped {code}: no broker_connection row (the app creates one at boot when credentials are present)");
            continue;
        }
        save_broker(pool, key, code, cred).await?;
        println!("  imported {code}");
        n += 1;
    }
    for (code, cred) in &feeds {
        save_feed(pool, key, code, cred).await?;
        println!("  imported {code}");
        n += 1;
    }

    println!(
        "\nimported {n} credential(s). You can now delete the matching lines from .env —\n\
         the environment is no longer consulted for broker or feed credentials."
    );
    Ok(n)
}
```

Add `pub mod import_env;` to `src/setup/mod.rs`.

- [ ] **Step 4: Register the subcommand**

In `src/main.rs`, add to `enum Command`:

```rust
    /// Credential store maintenance.
    #[command(subcommand)]
    Config(ConfigCmd),
```

and

```rust
#[derive(clap::Subcommand)]
enum ConfigCmd {
    /// Import broker and feed credentials from the environment into the store. Run once.
    ImportEnv,
}
```

Dispatch it beside the others: resolve the master key via `config::master_key(config::load())`, exit with a clear message if it is absent or invalid, build a pool with `setup::database_url()`, and call `setup::import_env::run`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test setup::import_env` then `cargo test`
Expected: 6 new tests pass; whole suite green.

- [ ] **Step 6: Commit**

```bash
git add src/setup/import_env.rs src/setup/mod.rs src/main.rs
git commit -m "feat(config): import-env moves credentials into the store

Skips anything half-configured — a key with no secret is a typo, and
storing it produces a credential that cannot authenticate, which then
reads as a broker outage rather than a missing value.

Binance's PEM is read at import time, while the operator is still there
to fix a bad path."
```

---

### Task 7: Boot from the store

**Files:**
- Modify: `src/main.rs`, `src/fix/mod.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: no new public symbols. `serve()` registers adapters from the database.

This is the task that changes behaviour. Read `src/main.rs`'s adapter-registration block and `src/fix/mod.rs`'s `start_ibkr` / `start_binance` in full before editing.

- [ ] **Step 1: Take the FIX starters off the environment**

`start_ibkr(env_name, …)` and `start_binance(env_name, …)` currently read `IBKR_{ENV}_*` / `BINANCE_{ENV}_*` via `env_opt`. Change each to take the already-resolved credential:

```rust
pub fn start_ibkr(
    env_name: &str,
    creds: &crate::credentials::BrokerCredentials,   // must be IbkrFix
    stream_health: &StreamHealthRegistry,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    position_changed_tx: Option<mpsc::Sender<()>>,
) -> Option<Arc<FixBrokerAdapter>>
```

Build `FixConfig` from the credential's fields instead of `env_opt`. For Binance, `private_key` is now PEM **contents** — pass it to `BinanceDialect::new` and `BinanceAdapter::new` directly rather than reading a file. Return `None` with an `error!` if the variant is not the expected one; that is a programming error, not operator input.

Delete `env_opt` if nothing else uses it.

- [ ] **Step 2: Replace the registration block in `serve()`**

Where `serve()` currently reads `ALPACA_*` and calls `fix::start_*`, instead:

```rust
    // Adapters come from the store, not the environment. One code path builds an
    // adapter, so a credential saved at runtime (Plan 3) and one loaded at boot
    // cannot diverge.
    let file_cfg = config::load();
    let master = match config::master_key(file_cfg) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => {
            error!("refusing to start: {e}");
            std::process::exit(1);
        }
        None => None,
    };

    // A missing key with credentials stored is fatal: starting "successfully"
    // with no brokers registered would misrepresent the system's state.
    if master.is_none() && credentials::any_credentials_stored(&pool).await.unwrap_or(false) {
        error!(
            "refusing to start: credentials are stored but no master key is configured. \
             Set oms.master_key in oms.toml (or OMS_MASTER_KEY)."
        );
        std::process::exit(1);
    }

    let mut registry = BrokerRegistry::new();
    for conn in credentials::load_brokers(&pool, master.as_ref()).await.unwrap_or_default() {
        if conn.status != "ACTIVE" {
            info!(code = %conn.code, "broker connection disabled, skipping");
            continue;
        }
        match &conn.credentials {
            credentials::CredentialState::Unconfigured => {
                info!(code = %conn.code, "no credentials stored, adapter not registered");
            }
            credentials::CredentialState::Error(e) => {
                error!(code = %conn.code, "credentials unusable: {e}");
            }
            credentials::CredentialState::Configured(c) => { /* register per variant */ }
        }
    }
```

In the `Configured` arm, match the variant: `Alpaca` → `registry.register_alpaca(env, Arc::new(AlpacaAdapter::new(key.clone(), secret.clone(), env)))`; `IbkrFix` → `fix::start_ibkr(...)`; `BinanceFix` → `fix::start_binance(...)`. Take the environment from `conn.environment`.

- [ ] **Step 2b: The Alpaca trade-update streams read the environment a second time**

`serve()` reads the `ALPACA_*` pairs again around `main.rs:750`/`:758` to spawn
`alpaca_stream::run`, which delivers execution reports. That is a separate read from
the adapter registration above. Leaving it on the environment means adapters come
from the store while execution reports stop arriving for a store-only credential.
Take the key and secret from the same `Configured` credential used to register the
adapter.

- [ ] **Step 3: Do the same for the Databento feed**

The feed is gated on `env::var("DATABENTO_API_KEY")` in `serve()`. Gate it on a `Configured` `FeedCredentials::Databento` from `load_feeds` instead.

**`opra_stream.rs:77` also reads the variable independently** — it calls
`LiveClient::builder().key_from_env()`. Gating `serve()` alone would leave the feed
still authenticating from the environment, so this must change too: give
`DatabentoOpraFeed` an api-key field, thread the stored key through its
constructor, and replace `key_from_env()` with the explicit `.key(...)` builder
method. Check the `databento` crate's builder for the exact method name.

- [ ] **Step 4: Update `bootstrap::has_creds`**

`src/setup/brokers.rs`'s `Broker::has_creds()` is a literal `env::var` check, and `bootstrap::will_sync_on_boot` depends on it. Change it to ask the store — a broker "has credentials" when its connection row holds a `Configured` credential.

Two notes. `Broker::ALL` is `[Alpaca, Binance]` only, so IBKR is not part of this
path. And `has_creds()` is currently synchronous while the store is async: either
make it async and update `will_sync_on_boot`/`ensure_broker_connections`, or pass
the already-loaded connection list in. Prefer passing the list — `serve()` has it
by then, and it avoids a second round trip.

- [ ] **Step 5: Verify against a live database**

```bash
docker run -d --name oms-t7 -e POSTGRES_PASSWORD=secret -p 55436:5432 postgres:16
sleep 4
mkdir -p /tmp/oms-t7 && cd /tmp/oms-t7
oms init --non-interactive   # with POSTGRES_* pointing at :55436
# no credentials stored, no key needed:
oms                          # must start, log "no credentials stored", and bind
# then import a fake Alpaca pair and confirm it registers:
ALPACA_PAPER_API_KEY=x ALPACA_PAPER_API_SECRET=y oms config import-env
oms                          # must log "registered ALPACA/PAPER"
docker rm -f oms-t7
```

Expected: starts cleanly with nothing stored; registers the adapter after import. (The fake credential will fail its first API call — that is fine and expected; registration is what this step proves.)

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/fix/mod.rs src/setup/brokers.rs
git commit -m "feat(adapters): register brokers and feeds from the credential store

serve() no longer reads {BROKER}_{ENV}_* from the environment. One code
path builds an adapter, so a credential saved at runtime later and one
loaded at boot cannot diverge.

A missing master key with credentials stored is fatal — starting with no
adapters registered would misrepresent the system's state. An
undecryptable credential is logged per connection and skipped, not
silently treated as absent."
```

---

### Task 8: `oms config rotate-key`

**Files:**
- Modify: `src/setup/import_env.rs` (or a sibling `src/setup/rotate.rs`), `src/main.rs`

**Interfaces:**
- Produces: `pub async fn rotate(pool: &PgPool, old: &MasterKey, new: &MasterKey) -> Result<usize, sqlx::Error>`

- [ ] **Step 1: Write the failing test**

The re-wrap logic is pure enough to test through the seal/open seam:

```rust
    /// Rotation re-wraps under the new key without touching the credential itself,
    /// and the AAD stays the connection code so the binding survives.
    #[test]
    fn rewrap_preserves_the_plaintext_and_the_binding() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        let sealed_old = crate::secrets::seal(&old, "alpaca-paper", b"payload");

        let sealed_new = rewrap(&old, &new, "alpaca-paper", &sealed_old).expect("rewrap");

        assert_eq!(crate::secrets::open(&new, "alpaca-paper", &sealed_new).expect("open"), b"payload");
        assert!(crate::secrets::open(&old, "alpaca-paper", &sealed_new).is_err(), "old key must stop working");
        assert!(crate::secrets::open(&new, "alpaca-live", &sealed_new).is_err(), "binding must survive");
    }

    /// A blob that will not open under the old key must abort the rotation rather
    /// than be dropped — losing one credential silently is worse than failing.
    #[test]
    fn rewrap_refuses_a_blob_it_cannot_open() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        assert!(rewrap(&old, &new, "alpaca-paper", &[0u8; 40]).is_err());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test rewrap`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
/// Open under `old` and re-seal under `new`, keeping `code` as the AAD.
pub fn rewrap(
    old: &MasterKey,
    new: &MasterKey,
    code: &str,
    sealed: &[u8],
) -> Result<Vec<u8>, crate::secrets::SecretError> {
    let plain = crate::secrets::open(old, code, sealed)?;
    Ok(crate::secrets::seal(new, code, &plain))
}
```

Then `rotate`: read every row with a non-null `credentials`, `rewrap` each, and write them back **inside one transaction**, so a failure part way leaves every row still readable under the old key. Abort the whole rotation if any row fails to open — a partially rotated store has no single key that reads all of it.

Print the new key at the end with an instruction to replace `oms.master_key` in `oms.toml`, and warn that the old key must be kept until that is done.

- [ ] **Step 4: Register the subcommand**

Add `RotateKey` to `ConfigCmd`. It reads the current key from config, generates a new one with `setup::init::generate_master_key`, rotates, and prints the new value.

- [ ] **Step 5: Verify**

Run: `cargo test` — expect the suite green with the two new tests.

- [ ] **Step 6: Commit**

```bash
git add src/setup src/main.rs
git commit -m "feat(config): rotate-key re-wraps every stored credential

One transaction, and any row that will not open aborts the whole
rotation: a partially rotated store has no single key that reads all of
it, which is worse than not rotating."
```

---

### Task 9: Documentation

**Files:**
- Modify: `readme.md`, `.env.example`

- [ ] **Step 1: Document the store**

Add a **Broker and feed credentials** section to `readme.md`: credentials live encrypted in Postgres, sealed with the master key in `oms.toml`; the environment is no longer read for them; `oms config import-env` is the one-shot migration; `oms config rotate-key` re-wraps. State plainly that losing `oms.toml` loses every stored credential.

- [ ] **Step 2: Strip the credential keys from `.env.example`**

Delete `ALPACA_*`, `BINANCE_*`, `DATABENTO_API_KEY` and replace them with a comment pointing at `oms config import-env` and the cockpit (once Plan 4 lands). Leave the bootstrap and Kafka keys.

- [ ] **Step 3: Verify the claims against the code**

Re-read what you wrote against `src/main.rs`'s registration block. Any statement about what is read from the environment must match reality.

- [ ] **Step 4: Commit**

```bash
git add readme.md .env.example
git commit -m "docs: credentials live in the database, not the environment"
```

---

## Verification

```bash
cargo build          # no new warnings
cargo test           # 186 existing + ~33 new
```

End-to-end acceptance, against a throwaway Postgres:

1. `oms init` → `oms` starts with no credentials stored and no master key needed.
2. `oms config import-env` with `ALPACA_PAPER_*` set → reports one import.
3. `oms` → logs `registered ALPACA/PAPER`, with the environment variables **unset**.
4. Remove `oms.master_key` from `oms.toml` → `oms` refuses to start, naming the key.
5. `oms config rotate-key`, update `oms.toml`, restart → still registers.

## Out of scope

- **Plan 3** — live reload. Until it lands, a credential change needs a restart; the store is read once at boot.
- **Plan 4** — the credential endpoints and cockpit screens. `Redacted` exists for them but nothing serves it yet.
- **Kafka and OpenFIGI credentials.** Same shape, deliberately deferred; they drop in once this pattern exists.
- **User accounts.** `credentials_updated_by` stays null.
