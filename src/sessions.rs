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
use axum::http::{HeaderMap, Method};
use sqlx::{PgPool, Row};
use uuid::Uuid;

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

#[derive(Clone)]
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
#[derive(Clone)]
pub struct CookiePolicy {
    pub secure: bool,
}

/// The pieces of session handling that are fixed once at boot rather than
/// recomputed per request: the cookie policy is derived from the bind address
/// (see `cookie_policy`), and the TTLs govern idle/absolute expiry. Held on
/// `AppState` as a single field so a request never redoes this derivation.
///
/// `ttl` is `SessionTtl::default()` for now; a later task sources it from
/// configuration instead.
#[derive(Clone)]
pub struct SessionConfig {
    pub cookie_policy: CookiePolicy,
    pub ttl: SessionTtl,
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

/// Reject state-changing requests that did not come from our own origin.
///
/// `SameSite=Lax` already blocks the classic cross-site form post; this closes
/// what it does not cover and is why there is no CSRF token anywhere in this
/// design. Reads are exempt: they change nothing, and demanding an `Origin` on
/// GET would break ordinary links into the app.
pub fn origin_is_allowed(headers: &HeaderMap, method: &Method, public_base_url: &str) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin.trim_end_matches('/') == public_base_url.trim_end_matches('/')
}

pub struct SessionRecord {
    pub id: Uuid,
    pub principal_id: Uuid,
    pub principal_code: String,
    pub last_seen_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
}

pub async fn create_session(
    pool: &PgPool,
    principal_id: Uuid,
    ttl: &SessionTtl,
    user_agent: Option<&str>,
) -> Result<SessionToken, sqlx::Error> {
    let token = generate_session_token();
    sqlx::query(
        "INSERT INTO user_session (principal_id, token_hash, absolute_expires_at, user_agent) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(principal_id)
    .bind(&token.hash)
    .bind(absolute_expiry(Utc::now(), ttl))
    .bind(user_agent)
    .execute(pool)
    .await?;
    Ok(token)
}

/// Resolve a cookie value to its session, joining `principal` so a disabled
/// principal's sessions stop resolving — the same rule `verify_key` applies to
/// API keys.
pub async fn lookup_session(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<SessionRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT s.id, s.principal_id, p.code AS principal_code, \
                s.last_seen_at, s.absolute_expires_at \
         FROM user_session s \
         JOIN principal p ON p.id = s.principal_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND p.status = 'ACTIVE'",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| SessionRecord {
        id: r.get("id"),
        principal_id: r.get("principal_id"),
        principal_code: r.get("principal_code"),
        last_seen_at: r.get("last_seen_at"),
        absolute_expires_at: r.get("absolute_expires_at"),
    }))
}

pub async fn touch_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE user_session SET last_seen_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn revoke_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE user_session SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn revoke_all_for_principal(
    pool: &PgPool,
    principal_id: Uuid,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE user_session SET revoked_at = now() \
         WHERE principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(principal_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete sessions past their absolute cap. Idle expiry is enforced on read, so
/// this only clears what can never resolve again.
pub async fn sweep_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM user_session WHERE absolute_expires_at < now()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn ttl() -> SessionTtl {
        SessionTtl { idle: Duration::minutes(30), absolute: Duration::hours(12) }
    }

    fn with_origin(origin: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("origin", origin.parse().unwrap());
        headers
    }

    #[test]
    fn a_post_from_our_own_origin_is_allowed() {
        let headers = with_origin("https://oms.example.com");

        assert!(origin_is_allowed(&headers, &Method::POST, "https://oms.example.com"));
    }

    #[test]
    fn a_post_from_somewhere_else_is_refused() {
        let headers = with_origin("https://evil.example.com");

        assert!(!origin_is_allowed(&headers, &Method::POST, "https://oms.example.com"));
    }

    #[test]
    fn a_state_changing_request_with_no_origin_at_all_is_refused() {
        assert!(!origin_is_allowed(&HeaderMap::new(), &Method::POST, "https://oms.example.com"));
    }

    #[test]
    fn a_read_is_not_gated_on_origin() {
        // GET is not state-changing, and SameSite=Lax already governs top-level
        // navigation. Refusing origin-less GETs would break ordinary links.
        assert!(origin_is_allowed(&HeaderMap::new(), &Method::GET, "https://oms.example.com"));
    }

    #[test]
    fn a_trailing_slash_in_the_configured_url_does_not_break_the_match() {
        let headers = with_origin("https://oms.example.com");

        assert!(origin_is_allowed(&headers, &Method::POST, "https://oms.example.com/"));
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

    /// Round trip against a real database. The store is nothing but SQL, so a
    /// test without Postgres would only be testing sqlx.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live Postgres reachable via the usual POSTGRES_* config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_session_lives_until_it_is_revoked() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "session-store-test").await;

        let token = create_session(&pool, principal_id, &SessionTtl::default(), Some("test-agent"))
            .await
            .expect("create");

        let found = lookup_session(&pool, &token.hash).await.expect("lookup").expect("present");
        assert_eq!(found.principal_id, principal_id);
        assert_eq!(found.principal_code, principal_code);

        touch_session(&pool, found.id).await.expect("touch");
        revoke_session(&pool, found.id).await.expect("revoke");

        assert!(
            lookup_session(&pool, &token.hash).await.expect("lookup").is_none(),
            "a revoked session must not resolve"
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_disabled_principal_loses_its_sessions() {
        let pool = test_pool().await;
        let (principal_id, _principal_code) = seed_principal(&pool, "session-disable-test").await;
        let token = create_session(&pool, principal_id, &SessionTtl::default(), None)
            .await
            .expect("create");

        assert!(
            lookup_session(&pool, &token.hash).await.expect("lookup").is_some(),
            "the session must resolve while the principal is still active"
        );

        sqlx::query("UPDATE principal SET status = 'DISABLED' WHERE id = $1")
            .bind(principal_id)
            .execute(&pool)
            .await
            .expect("disable");

        assert!(lookup_session(&pool, &token.hash).await.expect("lookup").is_none());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn revoking_a_principal_kills_every_one_of_its_sessions() {
        let pool = test_pool().await;
        let (principal_id, _principal_code) = seed_principal(&pool, "session-revoke-all-test").await;
        let a = create_session(&pool, principal_id, &SessionTtl::default(), None).await.expect("a");
        let b = create_session(&pool, principal_id, &SessionTtl::default(), None).await.expect("b");

        let killed = revoke_all_for_principal(&pool, principal_id).await.expect("revoke all");

        assert_eq!(killed, 2);
        assert!(lookup_session(&pool, &a.hash).await.expect("a").is_none());
        assert!(lookup_session(&pool, &b.hash).await.expect("b").is_none());
    }

    // ── test plumbing ────────────────────────────────────────────────────────

    /// `main` loads .env before resolving config; a test binary does not, so
    /// without this the test resolves a different database than the server runs
    /// against. The `oms` role carries `search_path = oms, public`
    /// (db/access/roles.sql); these are the admin credentials, so set it here.
    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// `principal.code` is `UNIQUE` and these tests leave their rows behind, so
    /// the code is suffixed with the row's own id. Returns the id and the code
    /// actually inserted, since callers assert against it.
    async fn seed_principal(pool: &sqlx::PgPool, code: &str) -> (Uuid, String) {
        let id = Uuid::new_v4();
        let code = format!("{code}-{id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')",
        )
        .bind(id)
        .bind(&code)
        .execute(pool)
        .await
        .expect("seed principal");
        (id, code)
    }
}
