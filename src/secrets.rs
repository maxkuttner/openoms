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
