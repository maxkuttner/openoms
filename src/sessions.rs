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
use axum::http::HeaderMap;

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

/// Whether this deployment can carry a `Secure`, `__Host-`-prefixed cookie.
///
/// Both require HTTPS, which plain-http localhost cannot satisfy — so a loopback
/// bind drops them, and anything else requires them. Same reasoning as the
/// default-admin-password rule in `main.rs`.
pub struct CookiePolicy {
    pub secure: bool,
}

pub fn cookie_policy(bind_addr: &str) -> CookiePolicy {
    let host = bind_addr.rsplit_once(':').map_or(bind_addr, |(h, _)| h);
    let loopback = host == "localhost"
        || host == "::1"
        || host == "[::1]"
        || host.starts_with("127.");
    CookiePolicy { secure: !loopback }
}

pub fn cookie_name(policy: &CookiePolicy) -> &'static str {
    if policy.secure { "__Host-oms_session" } else { "oms_session" }
}

pub fn set_cookie_header(policy: &CookiePolicy, value: &str, max_age: Duration) -> String {
    let mut header = format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        cookie_name(policy),
        max_age.num_seconds()
    );
    if policy.secure {
        header.push_str("; Secure");
    }
    header
}

pub fn clear_cookie_header(policy: &CookiePolicy) -> String {
    set_cookie_header(policy, "", Duration::zero())
}

pub fn cookie_from_headers(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
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

    #[test]
    fn a_public_bind_demands_the_host_prefix_and_secure() {
        let policy = cookie_policy("0.0.0.0:3001");

        assert!(policy.secure);
        assert_eq!(cookie_name(&policy), "__Host-oms_session");

        let header = set_cookie_header(&policy, "abc", Duration::hours(12));
        assert!(header.starts_with("__Host-oms_session=abc;"));
        assert!(header.contains("Secure"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=43200"));
    }

    #[test]
    fn a_loopback_bind_drops_the_prefix_and_secure_because_http_cannot_carry_them() {
        let policy = cookie_policy("localhost:3001");

        assert!(!policy.secure);
        assert_eq!(cookie_name(&policy), "oms_session");

        let header = set_cookie_header(&policy, "abc", Duration::hours(12));
        assert!(header.starts_with("oms_session=abc;"));
        assert!(!header.contains("Secure"));
        assert!(header.contains("HttpOnly"), "HttpOnly is not negotiable on loopback either");
    }

    #[test]
    fn the_127_form_is_loopback_too() {
        assert!(!cookie_policy("127.0.0.1:3001").secure);
    }

    #[test]
    fn clearing_the_cookie_expires_it_immediately() {
        let header = clear_cookie_header(&cookie_policy("localhost:3001"));

        assert!(header.contains("Max-Age=0"));
        assert!(header.starts_with("oms_session=;"));
    }

    #[test]
    fn the_cookie_is_found_among_its_neighbours() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", "theme=dark; oms_session=wanted; other=x".parse().unwrap());

        assert_eq!(cookie_from_headers(&headers, "oms_session").as_deref(), Some("wanted"));
    }

    #[test]
    fn a_name_that_merely_ends_with_the_cookie_name_is_not_a_match() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", "not_oms_session=wrong".parse().unwrap());

        assert_eq!(cookie_from_headers(&headers, "oms_session"), None);
    }

    #[test]
    fn no_cookie_header_is_not_an_error() {
        assert_eq!(cookie_from_headers(&axum::http::HeaderMap::new(), "oms_session"), None);
    }
}
