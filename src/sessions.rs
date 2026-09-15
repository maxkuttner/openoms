//! Browser sessions for humans who signed in through the identity provider.
//!
//! An API key is a machine credential: long-lived, kept in an environment
//! variable, verified with bcrypt. A session is the opposite — short-lived, held
//! in a cookie the browser cannot read, and checked on every single interaction.
//! That difference drives every choice in this file.

use chrono::{DateTime, Duration, Utc};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Written to the browser once; only its hash is ever stored.
pub struct SessionToken {
    pub plaintext: String,
    pub hash: String,
}

/// Writes are throttled to this, so a polling blotter does not turn every
/// request into a row update.
const TOUCH_INTERVAL_SECONDS: i64 = 60;

pub fn generate_session_token() -> SessionToken {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let plaintext = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_session_token(&plaintext);
    SessionToken { plaintext, hash }
}

/// SHA-256, not bcrypt — deliberately unlike `api_key`.
///
/// bcrypt exists to slow the guessing of low-entropy secrets, and is affordable
/// at API-call rates. This value is 256 bits of randomness and is verified on
/// every interaction, so the right cost is one indexed lookup.
pub fn hash_session_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct SessionTtl {
    pub idle: Duration,
    pub absolute: Duration,
}

impl Default for SessionTtl {
    fn default() -> Self {
        Self { idle: Duration::minutes(30), absolute: Duration::hours(12) }
    }
}

pub fn absolute_expiry(now: DateTime<Utc>, ttl: &SessionTtl) -> DateTime<Utc> {
    now + ttl.absolute
}

pub fn is_expired(
    now: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    absolute_expires_at: DateTime<Utc>,
    ttl: &SessionTtl,
) -> bool {
    now >= absolute_expires_at || now - last_seen_at >= ttl.idle
}

pub fn needs_touch(now: DateTime<Utc>, last_seen_at: DateTime<Utc>) -> bool {
    (now - last_seen_at).num_seconds() > TOUCH_INTERVAL_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl() -> SessionTtl {
        SessionTtl { idle: Duration::minutes(30), absolute: Duration::hours(12) }
    }

    #[test]
    fn a_generated_token_is_unguessable_and_stored_only_as_its_hash() {
        let a = generate_session_token();
        let b = generate_session_token();

        assert_ne!(a.plaintext, b.plaintext, "two sessions must not collide");
        // 256 bits, base64url, unpadded.
        assert_eq!(a.plaintext.len(), 43);
        assert!(!a.plaintext.contains('='));
        assert_eq!(a.hash, hash_session_token(&a.plaintext));
        assert_ne!(a.hash, a.plaintext, "the plaintext must never be the stored value");
    }

    #[test]
    fn an_idle_session_expires_before_its_absolute_cap() {
        let now = Utc::now();
        let last_seen = now - Duration::minutes(31);
        let absolute = now + Duration::hours(6);

        assert!(is_expired(now, last_seen, absolute, &ttl()));
    }

    #[test]
    fn a_busy_session_still_dies_at_its_absolute_cap() {
        let now = Utc::now();
        let last_seen = now - Duration::seconds(5);
        let absolute = now - Duration::seconds(1);

        assert!(is_expired(now, last_seen, absolute, &ttl()));
    }

    #[test]
    fn a_recently_used_session_within_its_cap_is_live() {
        let now = Utc::now();

        assert!(!is_expired(now, now - Duration::minutes(5), now + Duration::hours(6), &ttl()));
    }

    #[test]
    fn last_seen_is_written_at_most_once_a_minute() {
        let now = Utc::now();

        assert!(!needs_touch(now, now - Duration::seconds(30)));
        assert!(needs_touch(now, now - Duration::seconds(61)));
    }

    #[test]
    fn the_absolute_expiry_is_the_cap_from_now() {
        let now = Utc::now();

        assert_eq!(absolute_expiry(now, &ttl()), now + Duration::hours(12));
    }
}
