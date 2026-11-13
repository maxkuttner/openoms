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

#[derive(Clone, Debug)]
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
/// Both require HTTPS, which plain-http localhost cannot satisfy — so a purely
/// loopback plain-http deployment drops them, and anything else requires them.
/// Same reasoning as the default-admin-password rule in `main.rs`.
#[derive(Clone)]
pub struct CookiePolicy {
    pub secure: bool,
}

/// The pieces of session handling that are fixed once at boot rather than
/// recomputed per request: the cookie policy is derived from `public_base_url`
/// (falling back to the bind address — see `cookie_policy`), and the TTLs
/// govern idle/absolute expiry. Held on
/// `AppState` as a single field so a request never redoes this derivation.
///
/// `ttl` is `SessionTtl::default()` for now; a later task sources it from
/// configuration instead.
///
/// `public_base_url` is the CSRF origin check's expected value (see
/// `origin_is_allowed`). It must be a *configured* value, never derived from
/// a request's own `Host` header — a `Host`-derived expectation moves with
/// whatever `Host` an attacker sends (DNS rebinding, a proxy forwarding an
/// attacker-controlled `Host`), so the check would always pass. `None` for
/// now — a later task sources it from OIDC configuration, which is also the
/// only place a session can ever be minted (see `auth::auth_middleware`).
#[derive(Clone)]
pub struct SessionConfig {
    pub cookie_policy: CookiePolicy,
    pub ttl: SessionTtl,
    pub public_base_url: Option<String>,
}

/// What the browser sees decides this, not what the process bound to.
///
/// The bind address is the wrong authority whenever a TLS terminator sits in
/// front: the standard production topology is nginx or Caddy holding the
/// certificate and proxying to `127.0.0.1:3001`, which makes the bind loopback
/// and the site `https://` all the same. Deciding from the bind there would
/// drop `Secure` and the `__Host-` prefix on a site served over HTTPS, and a
/// single forced `http://` navigation would then hand a network attacker a
/// live order-entry session.
///
/// So when OIDC is configured — `public_base_url` is `Some`, and it is the
/// value the IdP redirects back to, i.e. the browser's own view of this
/// server — its scheme decides. Plain http is honoured only for a genuine
/// loopback development host (`http://localhost`, `http://127.0.0.1`,
/// `http://[::1]`), which cannot carry `Secure` at all; anything else
/// demands it, including an unparseable URL, which fails closed.
///
/// With no OIDC configured there is no browser view to consult and no session
/// can ever be minted (only the OIDC callback mints one), so the bind address
/// is the only signal left and keeps its old meaning.
pub fn cookie_policy(bind_addr: &str, public_base_url: Option<&str>) -> CookiePolicy {
    match public_base_url {
        Some(url) => CookiePolicy { secure: !is_plain_http_loopback(url) },
        None => CookiePolicy { secure: !crate::bind_is_loopback(bind_addr) },
    }
}

/// Is this base URL plain `http://` on a host reachable only from this
/// machine — the one shape that legitimately cannot carry a `Secure` cookie?
///
/// Loopback is judged by `setup::database::config::is_loopback_host`, the same
/// exact-match list `main.rs`'s `bind_is_loopback` uses, so a host is never
/// loopback to one of these and public to the other. `Url::host_str` brackets
/// an IPv6 literal (`"[::1]"`), which that list also accepts.
///
/// Anything unparseable is *not* loopback: an address we cannot read must not
/// be what talks the server out of setting `Secure`.
pub fn is_plain_http_loopback(base_url: &str) -> bool {
    // `openidconnect::url` is the same `url` crate, re-exported — `oidc.rs`
    // reaches it the same way rather than adding a second direct dependency.
    let Ok(url) = openidconnect::url::Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(crate::setup::database::config::is_loopback_host)
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

/// Whether `method` is exempt from origin checks: it changes nothing, so
/// neither demanding an `Origin` on it nor refusing it for want of one buys
/// any security — only availability loss. The single source of truth for
/// that method list, so `origin_is_allowed` and any fail-closed caller (see
/// `auth::enforce_origin_for_session`) can't drift apart on which methods
/// count as "safe."
pub fn is_read_method(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Reject state-changing requests that did not come from our own origin.
///
/// `SameSite=Lax` already blocks the classic cross-site form post; this closes
/// what it does not cover and is why there is no CSRF token anywhere in this
/// design. Reads are exempt: they change nothing, and demanding an `Origin` on
/// GET would break ordinary links into the app.
pub fn origin_is_allowed(headers: &HeaderMap, method: &Method, public_base_url: &str) -> bool {
    if is_read_method(method) {
        return true;
    }
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let (origin, expected) = (origin.trim_end_matches('/'), public_base_url.trim_end_matches('/'));
    origin == expected || same_loopback_origin(origin, expected)
}

/// Two `http://` origins on the same port whose hosts are both loopback names
/// (`localhost`, `127.0.0.1`, `[::1]`). A browser sends whichever spelling the
/// user typed, and all of them reach this machine only.
fn same_loopback_origin(a: &str, b: &str) -> bool {
    use openidconnect::url::Url;
    let (Ok(a), Ok(b)) = (Url::parse(a), Url::parse(b)) else {
        return false;
    };
    is_plain_http_loopback(a.as_str())
        && is_plain_http_loopback(b.as_str())
        && a.port_or_known_default() == b.port_or_known_default()
}

/// The CSRF expected origin when `[auth].public_base_url` is not configured:
/// on a loopback bind, `http://<bind_addr>`; anywhere else `Err` with the
/// operator-facing refusal (the origin cannot be guessed safely, and local
/// login needs one).
pub fn derive_public_base_url(bind_addr: &str) -> Result<String, String> {
    if crate::bind_is_loopback(bind_addr) {
        Ok(format!("http://{bind_addr}"))
    } else {
        Err(format!(
            "refusing to start: local login is enabled, OMS_BIND_ADDR ({bind_addr}) is reachable              beyond this machine, and no public URL is configured. Set [auth].public_base_url in              oms.toml to the URL browsers use to reach this server (e.g. https://oms.example.com)."
        ))
    }
}

/// How a session was established — `user_session.auth_method`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    Oidc,
    Local,
    LegacyAdmin,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Oidc => "oidc",
            AuthMethod::Local => "local",
            AuthMethod::LegacyAdmin => "legacy_admin",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "local" => AuthMethod::Local,
            "legacy_admin" => AuthMethod::LegacyAdmin,
            _ => AuthMethod::Oidc,
        }
    }
}

pub struct SessionRecord {
    pub id: Uuid,
    pub principal_id: Uuid,
    pub principal_code: String,
    pub last_seen_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
    pub auth_method: AuthMethod,
    pub mfa_verified_at: Option<DateTime<Utc>>,
}

const INSERT_SESSION: &str = "INSERT INTO user_session \
     (principal_id, token_hash, absolute_expires_at, user_agent, auth_method, mfa_verified_at) \
     VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END) RETURNING id";

/// Mint a session. `mfa_verified` stamps `mfa_verified_at = now()`; a local
/// login starts without it and earns it at the TOTP step.
pub async fn create_session(
    pool: &PgPool,
    principal_id: Uuid,
    auth_method: AuthMethod,
    mfa_verified: bool,
    ttl: &SessionTtl,
    user_agent: Option<&str>,
) -> Result<SessionToken, sqlx::Error> {
    let token = generate_session_token();
    sqlx::query(INSERT_SESSION)
        .bind(principal_id)
        .bind(&token.hash)
        .bind(absolute_expiry(Utc::now(), ttl))
        .bind(user_agent)
        .bind(auth_method.as_str())
        .bind(mfa_verified)
        .execute(pool)
        .await?;
    Ok(token)
}

/// Replace session `old_id` with a fresh one in one transaction: the new row
/// is inserted and the old one revoked together, so the old token stops
/// working the moment the new one exists (session-fixation defence on a
/// privilege change such as passing MFA). `None` if `old_id` was already
/// revoked — a concurrent rotation won, and this one mints nothing.
pub async fn rotate_session(
    pool: &PgPool,
    old_id: Uuid,
    principal_id: Uuid,
    auth_method: AuthMethod,
    mfa_verified: bool,
    ttl: &SessionTtl,
    user_agent: Option<&str>,
) -> Result<Option<SessionToken>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let revoked = sqlx::query(
        "UPDATE user_session SET revoked_at = now() \
          WHERE id = $1 AND principal_id = $2 AND revoked_at IS NULL",
    )
    .bind(old_id)
    .bind(principal_id)
    .execute(&mut *tx)
    .await?;
    if revoked.rows_affected() != 1 {
        return Ok(None);
    }
    let token = generate_session_token();
    sqlx::query(INSERT_SESSION)
        .bind(principal_id)
        .bind(&token.hash)
        .bind(absolute_expiry(Utc::now(), ttl))
        .bind(user_agent)
        .bind(auth_method.as_str())
        .bind(mfa_verified)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(token))
}

/// Revoke every live session of `principal_id` (logout-all, credential
/// change). Returns how many were revoked.
pub async fn revoke_all_for_principal(pool: &PgPool, principal_id: Uuid) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE user_session SET revoked_at = now() WHERE principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(principal_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Resolve a cookie value to its session, joining `principal` so a disabled
/// principal's sessions stop resolving — the same rule `verify_key` applies to
/// API keys. Also requires `principal_type = 'HUMAN'`: a SERVICE or STRATEGY
/// principal must never hold a browser session, even if a row somehow carries
/// a live `user_session` for it.
pub async fn lookup_session(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<SessionRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT s.id, s.principal_id, p.code AS principal_code, \
                s.last_seen_at, s.absolute_expires_at, s.auth_method, s.mfa_verified_at \
         FROM user_session s \
         JOIN principal p ON p.id = s.principal_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND p.status = 'ACTIVE' \
               AND p.principal_type = 'HUMAN'",
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
        auth_method: AuthMethod::parse(r.get::<&str, _>("auth_method")),
        mfa_verified_at: r.get("mfa_verified_at"),
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
    fn loopback_local_only_derives_expected_origin() {
        assert_eq!(derive_public_base_url("127.0.0.1:3001").unwrap(), "http://127.0.0.1:3001");
        assert_eq!(derive_public_base_url("localhost:3001").unwrap(), "http://localhost:3001");
        // Both spellings of the same loopback port are accepted.
        let derived = derive_public_base_url("127.0.0.1:3001").unwrap();
        for ok in ["http://localhost:3001", "http://127.0.0.1:3001"] {
            let mut h = HeaderMap::new();
            h.insert("origin", ok.parse().unwrap());
            assert!(origin_is_allowed(&h, &Method::POST, &derived), "{ok}");
        }
        for bad in ["http://localhost:3002", "https://localhost:3001", "http://evil.example.com:3001"] {
            let mut h = HeaderMap::new();
            h.insert("origin", bad.parse().unwrap());
            assert!(!origin_is_allowed(&h, &Method::POST, &derived), "{bad}");
        }
    }

    #[test]
    fn non_loopback_local_without_base_url_refuses_to_start() {
        let err = derive_public_base_url("0.0.0.0:3001").unwrap_err();
        assert!(err.contains("[auth].public_base_url"), "{err}");
    }

    #[test]
    fn derived_origin_gates_session_posts() {
        let cfg = SessionConfig {
            cookie_policy: cookie_policy("127.0.0.1:3001", Some("http://127.0.0.1:3001")),
            ttl: SessionTtl::default(),
            public_base_url: Some(derive_public_base_url("127.0.0.1:3001").unwrap()),
        };
        let base = cfg.public_base_url.as_deref().unwrap();
        let mut h = HeaderMap::new();
        h.insert("origin", "http://localhost:3001".parse().unwrap());
        assert!(origin_is_allowed(&h, &Method::POST, base));
        h.insert("origin", "http://attacker.example".parse().unwrap());
        assert!(!origin_is_allowed(&h, &Method::POST, base));
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
        let policy = cookie_policy("0.0.0.0:3001", None);

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
        let policy = cookie_policy("localhost:3001", None);

        assert!(!policy.secure);
        assert_eq!(cookie_name(&policy), "oms_session");

        let header = set_cookie_header(&policy, "abc", Duration::hours(12));
        assert!(header.starts_with("oms_session=abc;"));
        assert!(!header.contains("Secure"));
        assert!(header.contains("HttpOnly"), "HttpOnly is not negotiable on loopback either");
    }

    #[test]
    fn the_127_form_is_loopback_too() {
        assert!(!cookie_policy("127.0.0.1:3001", None).secure);
    }

    /// The IPv6 loopback forms, which had no coverage at all. An IPv6 literal
    /// must be bracketed to carry a port, so `[::1]:3001` is the spelling a
    /// real bind takes; `[::1]` is the port-less one.
    ///
    /// The unbracketed `::1:3001` resolves to the same answer only because
    /// splitting on the last colon happens to leave `::1` behind; it is
    /// asserted to agree with `bind_is_loopback` rather than left to chance,
    /// since that agreement is the point of sharing one implementation.
    #[test]
    fn the_ipv6_loopback_forms_are_loopback_too() {
        assert!(!cookie_policy("[::1]:3001", None).secure, "[::1]:3001");
        assert!(!cookie_policy("[::1]", None).secure, "[::1]");
        assert!(!cookie_policy("::1:3001", None).secure, "::1:3001");
        assert_eq!(cookie_policy("::1:3001", None).secure, !crate::bind_is_loopback("::1:3001"));

        // The same host as an OIDC public base URL, where brackets are
        // mandatory and `Url` hands them back bracketed.
        assert!(!cookie_policy("[::1]:3001", Some("http://[::1]:3001")).secure);
        assert!(cookie_policy("[::1]:3001", Some("https://[::1]:3001")).secure);
    }

    /// The whole of C2: the standard production topology is a TLS terminator
    /// in front and a loopback bind behind it. Judged by the bind alone, this
    /// site — served to browsers over HTTPS — would get a cookie with neither
    /// `Secure` nor the `__Host-` prefix.
    #[test]
    fn a_reverse_proxied_https_site_keeps_secure_behind_a_loopback_bind() {
        let policy = cookie_policy("127.0.0.1:3001", Some("https://oms.example.com"));

        assert!(policy.secure);
        assert_eq!(cookie_name(&policy), "__Host-oms_session");
        assert!(set_cookie_header(&policy, "abc", Duration::hours(12)).contains("Secure"));
    }

    /// The case the loopback exemption exists for, which must keep working:
    /// a developer running plain http on their own machine.
    #[test]
    fn plain_http_loopback_development_still_drops_secure() {
        for url in ["http://localhost:3001", "http://127.0.0.1:3001", "http://[::1]:3001"] {
            let policy = cookie_policy("127.0.0.1:3001", Some(url));

            assert!(!policy.secure, "{url} cannot carry Secure at all");
            assert_eq!(cookie_name(&policy), "oms_session", "{url}");
        }
    }

    /// Plain http on a host other people can reach is not development — it is
    /// a mistake, and the cookie demands `Secure` rather than going bare.
    /// (`main.rs` refuses to start on this config; the policy fails closed
    /// anyway rather than relying on that.)
    #[test]
    fn a_plain_http_public_host_still_demands_secure() {
        assert!(cookie_policy("127.0.0.1:3001", Some("http://oms.example.com")).secure);
    }

    /// A base URL we cannot read must not be what talks the server out of
    /// setting `Secure`.
    #[test]
    fn an_unreadable_public_base_url_fails_closed() {
        assert!(cookie_policy("127.0.0.1:3001", Some("not a url")).secure);
        assert!(cookie_policy("127.0.0.1:3001", Some("")).secure);
    }

    /// With no OIDC there is no browser view to consult, so the bind decides —
    /// and it must decide exactly as `bind_is_loopback` does, or a host would
    /// be loopback to the admin-password rule and public to the cookie rule.
    /// `127.0.0.2` is the address that used to split them: `starts_with("127.")`
    /// called it loopback, `bind_is_loopback` did not.
    #[test]
    fn the_bind_fallback_agrees_with_the_admin_password_rule() {
        for addr in ["localhost:3001", "127.0.0.1:3001", "[::1]:3001", "127.0.0.2:3001",
                     "0.0.0.0:3001", "192.168.1.10:3001", "oms.internal:3001", ""] {
            assert_eq!(
                cookie_policy(addr, None).secure,
                !crate::bind_is_loopback(addr),
                "{addr} judged differently by the two rules"
            );
        }
    }

    #[test]
    fn only_plain_http_on_a_loopback_host_is_exempt() {
        assert!(is_plain_http_loopback("http://localhost:3001"));
        assert!(is_plain_http_loopback("http://127.0.0.1:3001"));
        assert!(is_plain_http_loopback("http://[::1]:3001"));

        assert!(!is_plain_http_loopback("https://localhost:3001"), "https needs no exemption");
        assert!(!is_plain_http_loopback("http://127.0.0.2:3001"), "not the loopback address");
        assert!(!is_plain_http_loopback("http://oms.example.com"));
        assert!(!is_plain_http_loopback("garbage"));
    }

    #[test]
    fn clearing_the_cookie_expires_it_immediately() {
        let header = clear_cookie_header(&cookie_policy("localhost:3001", None));

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

        let token = create_session(&pool, principal_id, AuthMethod::Oidc, false, &SessionTtl::default(), Some("test-agent"))
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
        let token = create_session(&pool, principal_id, AuthMethod::Oidc, false, &SessionTtl::default(), None)
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
