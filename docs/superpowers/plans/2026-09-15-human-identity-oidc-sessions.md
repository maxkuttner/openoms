# Human Identity: OIDC Login and Sessions — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a human sign in to the OMS through the desk's own identity provider and act with the same authority an API token already grants.

**Architecture:** The OMS is a confidential OIDC relying party. It runs authorization code with PKCE, verifies the ID token, matches the `sub` claim to `principal.external_subject`, and mints its own server-side session behind an httpOnly cookie. Sessions and API keys both resolve to `AuthContext { principal_id, principal_code }`, so every existing handler, grant check and risk rule is untouched — the two credential kinds differ only at the door.

**Tech Stack:** Rust, axum 0.7, sqlx 0.8 / Postgres, `openidconnect` 4.x (discovery, token exchange, ID-token verification only), `sha2` for session-token hashing, `rand` for session and PKCE material, `secrets.rs` (AES-256-GCM) for the client secret.

**Spec:** `docs/superpowers/specs/2026-09-15-human-identity-oidc-sessions-design.md`

## Global Constraints

- **Off by default.** With no `[auth.oidc]` block in `oms.toml`, `/auth/*` returns 404 and nothing else changes. The `curl | sh` → `oms database init` → `oms` first run must stay untouched.
- **No password storage, ever.** No `password_hash` column, no local login path.
- **No IdP tokens are persisted.** Access and refresh tokens are discarded once identity is established.
- **Authorization does not change.** A session and an API token carrying the same `principal_id` have identical powers. Do not add grant logic, do not branch handler behaviour on credential kind.
- **Session cookie:** `__Host-oms_session`, `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/` off loopback; `oms_session` without the prefix or `Secure` on a loopback bind.
- **Session TTLs:** idle 30 minutes (sliding), absolute 12 hours. Both configurable.
- **The ID-token validator takes its keys as an argument.** Discovery and JWKS caching live outside it. This seam is mandatory — it is what makes the security-critical half testable without a network.
- **No wrapper crates** (`axum-oidc` and similar). They carry their own session model.
- **Admin surface is out of scope.** It keeps its single shared token.
- Every new `.sql` migration requires bumping the count in `src/setup/database/assets.rs::embeds_every_migration`.
- Any change to routes or response types requires regenerating `docs/openapi.json` (`cargo run -- openapi > docs/openapi.json`); a drift test at `src/main.rs:1346` enforces it.

## File Structure

| File | Responsibility |
| --- | --- |
| `src/sessions.rs` (new) | Session lifecycle: token generation and hashing, TTL maths, cookie construction and parsing, CSRF origin check, and the Postgres store. |
| `src/oidc.rs` (new) | Provider config, discovery and JWKS caching, and ID-token verification behind an injectable-keys seam. |
| `src/auth_api.rs` (new) | The four `/auth/*` handlers, PKCE/state round-trip, principal resolution and JIT provisioning. Mirrors the existing `credentials.rs` / `credentials_api.rs` split. |
| `src/auth.rs` (modify) | `AuthContext` gains `principal_code`; `auth_middleware` becomes a combined front trying session cookie then key material. |
| `src/config.rs` (modify) | `[auth.oidc]` section. |
| `src/handlers.rs` (modify) | Stamp `EventMetadata.actor` from `AuthContext` at lines 550, 713, 982. |
| `src/admin.rs` (modify) | Revoke every session for a principal. |
| `src/main.rs` (modify) | Router wiring, `ApiDoc` registration, expired-session sweep task. |
| `db/migrations/ods/oms/0024_CREATE_USER_SESSION_TABLE.sql` (new) | The `user_session` table. |

Flat modules, matching the repo's existing layout (`event_store.rs`, `credentials.rs`, `order_events.rs`). `src/auth.rs` is not restructured into a directory — it stays small because sessions and OIDC live beside it, not inside it.

---

### Task 1: Session token generation and TTL maths

**Files:**
- Create: `src/sessions.rs`
- Modify: `Cargo.toml`, `src/main.rs` (add `mod sessions;`)
- Test: in-module `#[cfg(test)]` in `src/sessions.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct SessionToken { pub plaintext: String, pub hash: String }`
  - `pub fn generate_session_token() -> SessionToken`
  - `pub fn hash_session_token(plaintext: &str) -> String`
  - `pub struct SessionTtl { pub idle: chrono::Duration, pub absolute: chrono::Duration }`
  - `impl Default for SessionTtl` — 30 minutes idle, 12 hours absolute
  - `pub fn absolute_expiry(now: DateTime<Utc>, ttl: &SessionTtl) -> DateTime<Utc>`
  - `pub fn is_expired(now: DateTime<Utc>, last_seen_at: DateTime<Utc>, absolute_expires_at: DateTime<Utc>, ttl: &SessionTtl) -> bool`
  - `pub fn needs_touch(now: DateTime<Utc>, last_seen_at: DateTime<Utc>) -> bool`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, under `[dependencies]`:

```toml
# Session cookie values are hashed, not bcrypted: see src/sessions.rs.
sha2 = "0.10"
```

- [ ] **Step 2: Write the failing tests**

Create `src/sessions.rs`:

```rust
//! Browser sessions for humans who signed in through the identity provider.
//!
//! An API key is a machine credential: long-lived, kept in an environment
//! variable, verified with bcrypt. A session is the opposite — short-lived, held
//! in a cookie the browser cannot read, and checked on every single interaction.
//! That difference drives every choice in this file.

use chrono::{DateTime, Duration, Utc};

/// Written to the browser once; only its hash is ever stored.
pub struct SessionToken {
    pub plaintext: String,
    pub hash: String,
}

pub fn generate_session_token() -> SessionToken {
    todo!("generate 256 bits and hash them")
}

pub fn hash_session_token(_plaintext: &str) -> String {
    todo!("sha-256, hex")
}

pub struct SessionTtl {
    pub idle: Duration,
    pub absolute: Duration,
}

pub fn absolute_expiry(_now: DateTime<Utc>, _ttl: &SessionTtl) -> DateTime<Utc> {
    todo!()
}

pub fn is_expired(
    _now: DateTime<Utc>,
    _last_seen_at: DateTime<Utc>,
    _absolute_expires_at: DateTime<Utc>,
    _ttl: &SessionTtl,
) -> bool {
    todo!()
}

pub fn needs_touch(_now: DateTime<Utc>, _last_seen_at: DateTime<Utc>) -> bool {
    todo!()
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
```

Add `mod sessions;` to `src/main.rs` beside the other module declarations.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --bin oms sessions`
Expected: FAIL — every test panics with `not yet implemented`.

- [ ] **Step 4: Implement**

```rust
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};

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
```

Delete the `todo!()` stubs as you replace them.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin oms sessions`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/sessions.rs src/main.rs
git commit -m "feat(sessions): session token generation and lifetime rules"
```

---

### Task 2: Session cookie construction and parsing

**Files:**
- Modify: `src/sessions.rs`
- Test: in-module `#[cfg(test)]` in `src/sessions.rs`

**Interfaces:**
- Consumes: `SessionToken` from Task 1.
- Produces:
  - `pub struct CookiePolicy { pub secure: bool }`
  - `pub fn cookie_policy(bind_addr: &str) -> CookiePolicy`
  - `pub fn cookie_name(policy: &CookiePolicy) -> &'static str`
  - `pub fn set_cookie_header(policy: &CookiePolicy, value: &str, max_age: chrono::Duration) -> String`
  - `pub fn clear_cookie_header(policy: &CookiePolicy) -> String`
  - `pub fn cookie_from_headers(headers: &axum::http::HeaderMap, name: &str) -> Option<String>`

Cookies are built and parsed by hand rather than pulling in a cookie crate: `auth.rs` already parses the `Authorization` header by hand, the attribute rules are the thing under test here, and the parsing is a dozen lines.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/sessions.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms sessions`
Expected: FAIL — `cannot find function cookie_policy`.

- [ ] **Step 3: Implement**

```rust
use axum::http::HeaderMap;

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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms sessions`
Expected: PASS, 13 tests.

- [ ] **Step 5: Commit**

```bash
git add src/sessions.rs
git commit -m "feat(sessions): build and read the session cookie"
```

---

### Task 3: CSRF origin check

**Files:**
- Modify: `src/sessions.rs`
- Test: in-module `#[cfg(test)]` in `src/sessions.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub fn origin_is_allowed(headers: &HeaderMap, method: &axum::http::Method, public_base_url: &str) -> bool`

A cookie is sent by the browser whether or not the page that triggered the request is ours. `SameSite=Lax` blocks the common cross-site form post; this is the second line, and it is what lets us skip CSRF tokens entirely.

- [ ] **Step 1: Write the failing tests**

```rust
    use axum::http::Method;

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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms sessions::tests::a_post`
Expected: FAIL — `cannot find function origin_is_allowed`.

- [ ] **Step 3: Implement**

```rust
use axum::http::Method;

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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms sessions`
Expected: PASS, 18 tests.

- [ ] **Step 5: Commit**

```bash
git add src/sessions.rs
git commit -m "feat(sessions): refuse state-changing requests from another origin"
```

---

### Task 4: The session store

**Files:**
- Create: `db/migrations/ods/oms/0024_CREATE_USER_SESSION_TABLE.sql`
- Modify: `src/sessions.rs`, `src/setup/database/assets.rs:96`
- Test: in-module `#[cfg(test)]` in `src/sessions.rs`, `#[ignore]`d

**Interfaces:**
- Consumes: `SessionToken`, `SessionTtl`, `absolute_expiry`, `hash_session_token` from Task 1.
- Produces:
  - `pub struct SessionRecord { pub id: Uuid, pub principal_id: Uuid, pub principal_code: String, pub last_seen_at: DateTime<Utc>, pub absolute_expires_at: DateTime<Utc> }`
  - `pub async fn create_session(pool: &PgPool, principal_id: Uuid, ttl: &SessionTtl, user_agent: Option<&str>) -> Result<SessionToken, sqlx::Error>`
  - `pub async fn lookup_session(pool: &PgPool, token_hash: &str) -> Result<Option<SessionRecord>, sqlx::Error>`
  - `pub async fn touch_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error>`
  - `pub async fn revoke_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error>`
  - `pub async fn revoke_all_for_principal(pool: &PgPool, principal_id: Uuid) -> Result<u64, sqlx::Error>`
  - `pub async fn sweep_expired(pool: &PgPool) -> Result<u64, sqlx::Error>`

- [ ] **Step 1: Write the migration**

Create `db/migrations/ods/oms/0024_CREATE_USER_SESSION_TABLE.sql`:

```sql
-- Browser sessions for humans authenticated at the identity provider.
--
-- `token_hash` is SHA-256, deliberately unlike `api_key.secret_hash`, which is
-- bcrypt. bcrypt slows the guessing of low-entropy secrets and is affordable at
-- API-call rates; this value is 256 bits of randomness checked on every single
-- interaction, so the right cost is one indexed lookup.
--
-- Two clocks: `last_seen_at` drives a sliding idle timeout, `absolute_expires_at`
-- is the hard cap so a desk re-authenticates at least daily.

CREATE TABLE user_session (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    principal_id        UUID NOT NULL REFERENCES principal(id),
    token_hash          TEXT NOT NULL UNIQUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    absolute_expires_at TIMESTAMPTZ NOT NULL,
    revoked_at          TIMESTAMPTZ,
    user_agent          TEXT,
    ip                  INET
);

CREATE INDEX idx_user_session_live ON user_session (token_hash) WHERE revoked_at IS NULL;
CREATE INDEX idx_user_session_principal ON user_session (principal_id);
```

- [ ] **Step 2: Bump the embedded migration count**

In `src/setup/database/assets.rs:96`, change the `oms` count from `23` to `24`. It is a pinned count; the test fails loudly otherwise.

- [ ] **Step 3: Write the failing test**

Append to the `tests` module in `src/sessions.rs`:

```rust
    /// Round trip against a real database. The store is nothing but SQL, so a
    /// test without Postgres would only be testing sqlx.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live Postgres reachable via the usual POSTGRES_* config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_session_lives_until_it_is_revoked() {
        let pool = test_pool().await;
        let principal_id = seed_principal(&pool, "session-store-test").await;

        let token = create_session(&pool, principal_id, &SessionTtl::default(), Some("test-agent"))
            .await
            .expect("create");

        let found = lookup_session(&pool, &token.hash).await.expect("lookup").expect("present");
        assert_eq!(found.principal_id, principal_id);
        assert_eq!(found.principal_code, "session-store-test");

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
        let principal_id = seed_principal(&pool, "session-disable-test").await;
        let token = create_session(&pool, principal_id, &SessionTtl::default(), None)
            .await
            .expect("create");

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
        let principal_id = seed_principal(&pool, "session-revoke-all-test").await;
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

    async fn seed_principal(pool: &sqlx::PgPool, code: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')",
        )
        .bind(id)
        .bind(format!("{code}-{id}"))
        .execute(pool)
        .await
        .expect("seed principal");
        id
    }
```

Note: `seed_principal` suffixes the code with a UUID because `principal.code` is `UNIQUE` and these tests leave their rows behind. Adjust the two `assert_eq!(found.principal_code, ...)` assertions to compare with the returned code rather than the bare literal — return the code from `seed_principal` as `(Uuid, String)` and assert against that.

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test --bin oms sessions -- --ignored`
Expected: FAIL — `cannot find function create_session`.

- [ ] **Step 5: Implement**

```rust
use sqlx::{PgPool, Row};
use uuid::Uuid;

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
```

- [ ] **Step 6: Apply the migration and run the tests**

Run: `cargo run -- database migrate`
Expected: `applying 0024_CREATE_USER_SESSION_TABLE.sql`, then `applied 1 migration(s)`.

Run: `cargo test --bin oms sessions -- --ignored`
Expected: PASS, 3 tests.

Run: `cargo test`
Expected: PASS — including `embeds_every_migration` with its new count.

- [ ] **Step 7: Commit**

```bash
git add db/migrations/ods/oms/0024_CREATE_USER_SESSION_TABLE.sql src/sessions.rs src/setup/database/assets.rs
git commit -m "feat(sessions): persist sessions, revocable per session or principal"
```

---

### Task 5: `AuthContext` carries the principal code, and the middleware accepts both doors

**Files:**
- Modify: `src/auth.rs:13-16` (`AuthContext`), `src/auth.rs:27-40` (`auth_middleware`), `src/auth.rs:45` (`verify_key`)
- Test: in-module `#[cfg(test)]` in `src/auth.rs`, `#[ignore]`d for the DB paths

**Interfaces:**
- Consumes: `cookie_policy`, `cookie_name`, `cookie_from_headers`, `hash_session_token`, `lookup_session`, `touch_session`, `needs_touch`, `is_expired`, `SessionTtl` from Tasks 1–4.
- Produces:
  - `pub struct AuthContext { pub principal_id: Uuid, pub principal_code: String }`
  - `pub async fn verify_key(pool: &PgPool, key_id: &str, secret: &str) -> Result<Option<(Uuid, String)>, Response>` — now returns the code alongside the id.

`AuthContext` gaining a field touches every construction site. There is exactly one (`src/auth.rs:38`); readers use `auth.principal_id` and are unaffected.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_session_cookie_authenticates_exactly_like_a_key() {
        let pool = test_pool().await;
        let (principal_id, code) = seed_principal(&pool).await;
        let token = crate::sessions::create_session(
            &pool,
            principal_id,
            &crate::sessions::SessionTtl::default(),
            None,
        )
        .await
        .expect("create session");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", format!("oms_session={}", token.plaintext).parse().unwrap());

        let ctx = authenticate(&pool, &headers, "localhost:3001")
            .await
            .expect("authenticate")
            .expect("a session should authenticate");

        assert_eq!(ctx.principal_id, principal_id);
        assert_eq!(ctx.principal_code, code, "the code must ride along, for the audit trail");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_idle_session_no_longer_authenticates() {
        let pool = test_pool().await;
        let (principal_id, _) = seed_principal(&pool).await;
        let token = crate::sessions::create_session(
            &pool,
            principal_id,
            &crate::sessions::SessionTtl::default(),
            None,
        )
        .await
        .expect("create session");

        // Backdate past the 30-minute idle window.
        sqlx::query("UPDATE user_session SET last_seen_at = now() - interval '31 minutes' \
                     WHERE token_hash = $1")
            .bind(&token.hash)
            .execute(&pool)
            .await
            .expect("backdate");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", format!("oms_session={}", token.plaintext).parse().unwrap());

        assert!(
            authenticate(&pool, &headers, "localhost:3001").await.expect("authenticate").is_none()
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn no_credential_of_either_kind_is_not_an_authentication() {
        let pool = test_pool().await;

        assert!(
            authenticate(&pool, &axum::http::HeaderMap::new(), "localhost:3001")
                .await
                .expect("authenticate")
                .is_none()
        );
    }
```

Reuse the `test_pool` and `seed_principal` helpers from Task 4 — copy them into `auth.rs`'s test module, returning `(Uuid, String)`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms auth -- --ignored`
Expected: FAIL — `cannot find function authenticate`.

- [ ] **Step 3: Implement**

Change `AuthContext` and extract the credential resolution so it is testable without building a full `Request`:

```rust
#[derive(Clone)]
pub struct AuthContext {
    pub principal_id: Uuid,
    /// The acting principal's `code`, carried so a handler can stamp an event's
    /// `actor` without a second query.
    pub principal_code: String,
}

/// Resolve whichever credential the request carries.
///
/// Session cookie first, key material second. Both produce the same context:
/// the difference between a human and a machine ends here, and no handler
/// downstream can tell them apart.
pub async fn authenticate(
    pool: &sqlx::PgPool,
    headers: &axum::http::HeaderMap,
    bind_addr: &str,
) -> Result<Option<AuthContext>, Response> {
    let policy = crate::sessions::cookie_policy(bind_addr);
    if let Some(value) = crate::sessions::cookie_from_headers(headers, crate::sessions::cookie_name(&policy)) {
        let hash = crate::sessions::hash_session_token(&value);
        if let Some(record) = crate::sessions::lookup_session(pool, &hash)
            .await
            .map_err(|_| service_unavailable())?
        {
            let ttl = crate::sessions::SessionTtl::default();
            let now = chrono::Utc::now();
            if crate::sessions::is_expired(now, record.last_seen_at, record.absolute_expires_at, &ttl) {
                return Ok(None);
            }
            if crate::sessions::needs_touch(now, record.last_seen_at) {
                crate::sessions::touch_session(pool, record.id)
                    .await
                    .map_err(|_| service_unavailable())?;
            }
            return Ok(Some(AuthContext {
                principal_id: record.principal_id,
                principal_code: record.principal_code,
            }));
        }
    }

    let Ok((key_id, secret)) = extract_trading_credentials(headers) else {
        return Ok(None);
    };
    Ok(verify_key(pool, &key_id, &secret)
        .await?
        .map(|(principal_id, principal_code)| AuthContext { principal_id, principal_code }))
}
```

`auth_middleware` becomes a thin wrapper over it:

```rust
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let ctx = authenticate(state.pool(), req.headers(), state.bind_addr())
        .await?
        .ok_or_else(unauthorized)?;
    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}
```

Change `verify_key`'s query to `SELECT k.principal_id, p.code, k.secret_hash` and its return type to `Result<Option<(Uuid, String)>, Response>`.

`extract_trading_credentials` currently takes `&HeaderMap` and returns `Result<_, Response>` — keep that signature; the `let Ok(...) else` above turns a missing credential into `None` rather than an error, so an anonymous request is a clean 401 rather than a 500.

`state.bind_addr()` may not exist on `AppState`; if not, add it as a stored `String` set at construction in `main.rs`, alongside `admin_token`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms auth -- --ignored`
Expected: PASS, 3 tests.

Run: `cargo test`
Expected: PASS — the whole suite, since `AuthContext`'s new field must compile everywhere.

- [ ] **Step 5: Commit**

```bash
git add src/auth.rs src/app_state.rs src/main.rs
git commit -m "feat(auth): accept a session cookie wherever a trading key works"
```

---

### Task 6: The `[auth.oidc]` configuration section

**Files:**
- Modify: `src/config.rs:22-32` (`FileConfig`), and a new `OidcSection` beside `ServerSection`
- Test: in-module `#[cfg(test)]` in `src/config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct OidcSection { pub issuer: Option<String>, pub client_id: Option<String>, pub public_base_url: Option<String>, pub scopes: Option<Vec<String>>, pub required_claim: Option<String>, pub required_claim_value: Option<String>, pub idle_ttl_minutes: Option<i64>, pub absolute_ttl_hours: Option<i64> }`
  - `pub fn oidc(&self) -> Option<OidcSettings>` on `FileConfig` — `None` when the block is absent or incomplete.
  - `pub struct OidcSettings { pub issuer: String, pub client_id: String, pub public_base_url: String, pub scopes: Vec<String>, pub required_claim: Option<(String, String)>, pub ttl: SessionTtl }`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn no_oidc_block_means_no_oidc() {
        let cfg: FileConfig = toml::from_str("[server]\nbind_addr = \"localhost:3001\"\n").unwrap();

        assert!(cfg.oidc().is_none(), "absent configuration must not half-enable login");
    }

    #[test]
    fn a_complete_block_produces_settings_with_defaults_filled_in() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [auth.oidc]
            issuer = "https://id.example.com"
            client_id = "oms"
            public_base_url = "https://oms.example.com"
            "#,
        )
        .unwrap();

        let settings = cfg.oidc().expect("settings");
        assert_eq!(settings.issuer, "https://id.example.com");
        assert_eq!(settings.scopes, vec!["openid", "profile", "email"]);
        assert_eq!(settings.ttl.idle, chrono::Duration::minutes(30));
        assert_eq!(settings.ttl.absolute, chrono::Duration::hours(12));
        assert!(settings.required_claim.is_none());
    }

    #[test]
    fn an_incomplete_block_is_refused_rather_than_half_applied() {
        let cfg: FileConfig = toml::from_str(
            "[auth.oidc]\nissuer = \"https://id.example.com\"\n",
        )
        .unwrap();

        assert!(cfg.oidc().is_none(), "issuer without client_id cannot log anyone in");
    }

    #[test]
    fn a_claim_gate_needs_both_halves_to_bind() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [auth.oidc]
            issuer = "https://id.example.com"
            client_id = "oms"
            public_base_url = "https://oms.example.com"
            required_claim = "groups"
            required_claim_value = "traders"
            "#,
        )
        .unwrap();

        let settings = cfg.oidc().expect("settings");
        assert_eq!(settings.required_claim, Some(("groups".into(), "traders".into())));
    }

    #[test]
    fn ttls_are_overridable() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [auth.oidc]
            issuer = "https://id.example.com"
            client_id = "oms"
            public_base_url = "https://oms.example.com"
            idle_ttl_minutes = 15
            absolute_ttl_hours = 8
            "#,
        )
        .unwrap();

        let settings = cfg.oidc().expect("settings");
        assert_eq!(settings.ttl.idle, chrono::Duration::minutes(15));
        assert_eq!(settings.ttl.absolute, chrono::Duration::hours(8));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms config`
Expected: FAIL — `no method named oidc`.

- [ ] **Step 3: Implement**

Add an `auth` field to `FileConfig` holding an `AuthSection { oidc: OidcSection }` so the TOML reads `[auth.oidc]`, keeping `#[serde(deny_unknown_fields)]` and `#[serde(default)]` as the neighbouring sections do. Then:

```rust
impl FileConfig {
    /// The OIDC settings, or `None` when login is not configured.
    ///
    /// Incomplete is treated as absent on purpose: a half-filled block would
    /// otherwise produce a login flow that cannot complete, and a 404 is a far
    /// clearer signal than a redirect into a broken exchange.
    pub fn oidc(&self) -> Option<OidcSettings> {
        let s = &self.auth.oidc;
        let (issuer, client_id, public_base_url) =
            (s.issuer.clone()?, s.client_id.clone()?, s.public_base_url.clone()?);

        Some(OidcSettings {
            issuer,
            client_id,
            public_base_url,
            scopes: s.scopes.clone().unwrap_or_else(|| {
                vec!["openid".into(), "profile".into(), "email".into()]
            }),
            required_claim: s
                .required_claim
                .clone()
                .zip(s.required_claim_value.clone()),
            ttl: SessionTtl {
                idle: chrono::Duration::minutes(s.idle_ttl_minutes.unwrap_or(30)),
                absolute: chrono::Duration::hours(s.absolute_ttl_hours.unwrap_or(12)),
            },
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms config`
Expected: PASS, 5 new tests plus the existing ones.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): an [auth.oidc] section, absent by default"
```

---

### Task 7: ID-token verification

**Files:**
- Create: `src/oidc.rs`
- Modify: `Cargo.toml`, `src/main.rs` (add `mod oidc;`)
- Test: in-module `#[cfg(test)]` in `src/oidc.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Expectations { pub issuer: String, pub audience: String, pub nonce: String, pub leeway: chrono::Duration }`
  - `pub struct VerifiedIdentity { pub subject: String, pub display_name: Option<String>, pub email: Option<String>, pub claims: serde_json::Value }`
  - `pub enum OidcError { Signature, Expired, WrongIssuer, WrongAudience, WrongNonce, Malformed, UnsupportedAlgorithm }`
  - `pub fn verify_id_token(token: &str, keys: &JsonWebKeySet, expected: &Expectations, now: DateTime<Utc>) -> Result<VerifiedIdentity, OidcError>`

**This is the security-critical task.** Every listed failure is a real-world OIDC vulnerability, and the keys are a parameter precisely so all of them can be tested without a network.

- [ ] **Step 1: Add the dependency**

```toml
# OIDC relying-party protocol: discovery, the code exchange, and ID-token
# verification. Not a session framework — sessions are ours (src/sessions.rs).
openidconnect = "4"
```

- [ ] **Step 2: Write the failing tests**

Create `src/oidc.rs` with the signature above stubbed as `todo!()`, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Mint a signing key and tokens in-process. No IdP, no network: the whole
    /// point of taking `keys` as an argument.
    fn signer() -> TestSigner { TestSigner::new() }

    fn expectations() -> Expectations {
        Expectations {
            issuer: "https://id.example.com".into(),
            audience: "oms".into(),
            nonce: "n-123".into(),
            leeway: chrono::Duration::seconds(60),
        }
    }

    #[test]
    fn a_well_formed_token_yields_its_subject() {
        let s = signer();
        let token = s.token_with(|c| c);

        let identity = verify_id_token(&token, &s.keys(), &expectations(), Utc::now())
            .expect("a valid token must verify");

        assert_eq!(identity.subject, "user-42");
    }

    #[test]
    fn a_token_signed_by_someone_else_is_refused() {
        let s = signer();
        let impostor = signer();
        let token = impostor.token_with(|c| c);

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::Signature)
        );
    }

    #[test]
    fn an_expired_token_is_refused_even_though_it_is_otherwise_perfect() {
        let s = signer();
        let token = s.token_with(|mut c| { c.exp = (Utc::now() - chrono::Duration::hours(1)).timestamp(); c });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::Expired)
        );
    }

    #[test]
    fn a_token_minted_for_another_application_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| { c.aud = "some-other-app".into(); c });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongAudience)
        );
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| { c.iss = "https://evil.example.com".into(); c });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongIssuer)
        );
    }

    #[test]
    fn a_replayed_token_from_a_different_login_is_refused() {
        let s = signer();
        let token = s.token_with(|mut c| { c.nonce = "some-other-nonce".into(); c });

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::WrongNonce)
        );
    }

    #[test]
    fn an_unsigned_token_is_refused_however_convincing_its_claims() {
        let s = signer();
        let token = s.unsigned_token();

        assert_eq!(
            verify_id_token(&token, &s.keys(), &expectations(), Utc::now()),
            Err(OidcError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn a_token_within_clock_leeway_still_verifies() {
        let s = signer();
        let token = s.token_with(|mut c| { c.exp = (Utc::now() - chrono::Duration::seconds(30)).timestamp(); c });

        assert!(verify_id_token(&token, &s.keys(), &expectations(), Utc::now()).is_ok());
    }
}
```

`TestSigner` is a test-only helper generating an RSA key pair, exposing `keys()` as a `JsonWebKeySet` and `token_with(f)` building a signed token whose claims `f` may mutate, plus `unsigned_token()` emitting `alg: none`. Build it with the key and JWT types `openidconnect` re-exports so the test signs what production verifies. `OidcError` needs `PartialEq` and `Debug` for these assertions.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --bin oms oidc`
Expected: FAIL — `not yet implemented`.

- [ ] **Step 4: Implement**

Implement `verify_id_token` over `openidconnect`'s ID-token verifier, mapping its error variants onto `OidcError`. Two rules the implementation must hold, both covered by the tests above: the algorithm comes from our allowed list rather than from the token's header, and `nonce` is compared to the expectation rather than merely being present.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin oms oidc`
Expected: PASS, 8 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/oidc.rs src/main.rs
git commit -m "feat(oidc): verify ID tokens, with the keys as an argument"
```

---

### Task 8: Discovery and JWKS caching

**Files:**
- Modify: `src/oidc.rs`
- Test: in-module `#[cfg(test)]` in `src/oidc.rs`

**Interfaces:**
- Consumes: `OidcSettings` (Task 6), `verify_id_token` (Task 7).
- Produces:
  - `pub struct Provider { /* settings, cached metadata, cached keys */ }`
  - `pub async fn Provider::discover(settings: OidcSettings, client_secret: String) -> Result<Provider, OidcError>`
  - `pub fn Provider::authorize_url(&self, state: &str, nonce: &str, pkce_challenge: &str) -> String`
  - `pub async fn Provider::exchange_code(&self, code: &str, pkce_verifier: &str, nonce: &str) -> Result<VerifiedIdentity, OidcError>`

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_authorize_url_carries_everything_the_provider_needs() {
        let provider = Provider::for_test("https://id.example.com", "oms", "https://oms.example.com");

        let url = provider.authorize_url("st-1", "n-1", "challenge-1");

        assert!(url.starts_with("https://id.example.com/authorize"));
        assert!(url.contains("client_id=oms"));
        assert!(url.contains("state=st-1"));
        assert!(url.contains("nonce=n-1"));
        assert!(url.contains("code_challenge=challenge-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Foms.example.com%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid"));
    }

    #[test]
    fn the_redirect_uri_is_derived_from_one_configured_value() {
        let provider = Provider::for_test("https://id.example.com", "oms", "https://oms.example.com/");

        assert_eq!(provider.redirect_uri(), "https://oms.example.com/auth/callback");
    }
```

`Provider::for_test` is a test-only constructor taking pre-baked metadata instead of performing discovery — discovery itself is exercised by the end-to-end Keycloak profile in Task 13, not by a unit test hitting the network.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms oidc::tests::the_authorize`
Expected: FAIL — `no function or associated item named for_test`.

- [ ] **Step 3: Implement**

Fetch `{issuer}/.well-known/openid-configuration` through `openidconnect`'s discovery with the existing `reqwest` client. Cache metadata and the JWKS on the `Provider`. Refetch the JWKS when verification fails on an unknown `kid`, rate-limited to at most once a minute so a malformed token cannot drive a fetch storm.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms oidc`
Expected: PASS, 10 tests.

- [ ] **Step 5: Commit**

```bash
git add src/oidc.rs
git commit -m "feat(oidc): discovery, cached keys, and the authorization URL"
```

---

### Task 9: Principal resolution and just-in-time provisioning

**Files:**
- Create: `src/auth_api.rs`
- Modify: `src/main.rs` (add `mod auth_api;`)
- Test: in-module `#[cfg(test)]` in `src/auth_api.rs`, `#[ignore]`d

**Interfaces:**
- Consumes: `VerifiedIdentity` (Task 7), `OidcSettings` (Task 6).
- Produces:
  - `pub enum ResolveOutcome { Resolved { principal_id: Uuid, principal_code: String }, ClaimRejected }`
  - `pub async fn resolve_or_provision(pool: &PgPool, identity: &VerifiedIdentity, required_claim: Option<&(String, String)>) -> Result<ResolveOutcome, sqlx::Error>`

- [ ] **Step 1: Write the failing tests**

```rust
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_first_login_provisions_a_human_with_no_grants() {
        let pool = test_pool().await;
        let identity = identity_for(&format!("sub-{}", Uuid::new_v4()));

        let outcome = resolve_or_provision(&pool, &identity, None).await.expect("resolve");

        let ResolveOutcome::Resolved { principal_id, .. } = outcome else {
            panic!("expected a principal");
        };
        let grants: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM principal_portfolio_grant WHERE principal_id = $1",
        )
        .bind(principal_id)
        .fetch_one(&pool)
        .await
        .expect("count grants");
        assert_eq!(grants, 0, "a new human must be able to do nothing until granted");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn logging_in_twice_does_not_create_a_second_principal() {
        let pool = test_pool().await;
        let identity = identity_for(&format!("sub-{}", Uuid::new_v4()));

        let first = resolve_or_provision(&pool, &identity, None).await.expect("first");
        let second = resolve_or_provision(&pool, &identity, None).await.expect("second");

        assert_eq!(principal_id_of(&first), principal_id_of(&second));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_existing_principal_is_matched_on_its_external_subject() {
        let pool = test_pool().await;
        let subject = format!("sub-{}", Uuid::new_v4());
        let expected = seed_principal_with_subject(&pool, &subject).await;

        let outcome = resolve_or_provision(&pool, &identity_for(&subject), None)
            .await
            .expect("resolve");

        assert_eq!(principal_id_of(&outcome), expected);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_claim_gate_keeps_the_rest_of_the_tenant_out() {
        let pool = test_pool().await;
        let mut identity = identity_for(&format!("sub-{}", Uuid::new_v4()));
        identity.claims = serde_json::json!({ "groups": "everyone-else" });

        let gate = ("groups".to_string(), "traders".to_string());
        let outcome = resolve_or_provision(&pool, &identity, Some(&gate)).await.expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::ClaimRejected));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_claim_gate_lets_the_intended_slice_through() {
        let pool = test_pool().await;
        let mut identity = identity_for(&format!("sub-{}", Uuid::new_v4()));
        identity.claims = serde_json::json!({ "groups": ["traders", "staff"] });

        let gate = ("groups".to_string(), "traders".to_string());
        let outcome = resolve_or_provision(&pool, &identity, Some(&gate)).await.expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
    }
```

Note the last two: the claim may be a string or an array of strings, and both shapes must be handled.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms auth_api -- --ignored`
Expected: FAIL — `cannot find function resolve_or_provision`.

- [ ] **Step 3: Implement**

```rust
/// Match an authenticated subject to its principal, creating one on first sight.
///
/// A provisioned principal has no grants, and a principal without grants can do
/// nothing: `require_order_grant` gates every order path and `/portfolios`
/// returns only granted rows. So first login yields an identity that can see
/// nothing until an admin grants it a portfolio — which is why this is safe to
/// do automatically, and why it beats making an admin hand-copy opaque subject
/// strings before anyone can log in.
pub async fn resolve_or_provision(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    required_claim: Option<&(String, String)>,
) -> Result<ResolveOutcome, sqlx::Error> {
    // ... existing lookup on external_subject; if found, return Resolved.
    // ... if not found: check the claim gate BEFORE inserting, so a rejected
    //     subject leaves no row behind. Then INSERT ... ON CONFLICT
    //     (external_subject) DO UPDATE SET display_name = EXCLUDED.display_name
    //     RETURNING id, code — the conflict clause is what makes two concurrent
    //     first logins idempotent rather than a unique-violation error.
}
```

The generated `code` must be unique and readable: derive it from the email local part or `display_name`, slugified, with a numeric suffix on collision.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms auth_api -- --ignored`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add src/auth_api.rs src/main.rs
git commit -m "feat(auth): resolve a subject to its principal, provisioning on first login"
```

---

### Task 10: The `/auth/*` endpoints

**Files:**
- Modify: `src/auth_api.rs`
- Test: in-module `#[cfg(test)]` in `src/auth_api.rs`

**Interfaces:**
- Consumes: `Provider` (Task 8), `resolve_or_provision` (Task 9), session store and cookie helpers (Tasks 1–4), `AuthContext` (Task 5).
- Produces:
  - `pub async fn login(...) -> Response` — 302 to the provider
  - `pub async fn callback(...) -> Result<Response, ApiError>`
  - `pub async fn logout(...) -> Result<Response, ApiError>`
  - `pub async fn me(...) -> Result<Json<MeResponse>, ApiError>`
  - `pub struct MeResponse { pub principal_id: String, pub code: String, pub display_name: Option<String>, pub portfolios: Vec<handlers::GrantedPortfolio> }`
  - `pub struct FlowState { pub state: String, pub nonce: String, pub pkce_verifier: String }`
  - `pub fn flow_cookie(policy: &CookiePolicy, flow: &FlowState) -> String`
  - `pub fn flow_from_cookie(headers: &HeaderMap, policy: &CookiePolicy) -> Option<FlowState>`

`state`, `nonce` and the PKCE verifier ride in a short-lived httpOnly cookie rather than a table: they are per-browser, worthless after the callback, and a table would need its own sweep.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_flow_cookie_survives_a_round_trip() {
        let policy = crate::sessions::cookie_policy("localhost:3001");
        let flow = FlowState {
            state: "st-1".into(),
            nonce: "n-1".into(),
            pkce_verifier: "v-1".into(),
        };

        let header = flow_cookie(&policy, &flow);
        let mut headers = HeaderMap::new();
        let value = header.split(';').next().unwrap().to_string();
        headers.insert("cookie", value.parse().unwrap());

        let read = flow_from_cookie(&headers, &policy).expect("flow");
        assert_eq!(read.state, "st-1");
        assert_eq!(read.nonce, "n-1");
        assert_eq!(read.pkce_verifier, "v-1");
    }

    #[test]
    fn the_flow_cookie_is_short_lived_and_unreadable_to_script() {
        let policy = crate::sessions::cookie_policy("localhost:3001");
        let header = flow_cookie(&policy, &FlowState {
            state: "st-1".into(), nonce: "n-1".into(), pkce_verifier: "v-1".into(),
        });

        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Max-Age=300"));
    }

    #[test]
    fn a_callback_with_no_flow_cookie_cannot_be_trusted() {
        assert!(flow_from_cookie(&HeaderMap::new(), &crate::sessions::cookie_policy("localhost:3001")).is_none());
    }

    #[test]
    fn a_state_mismatch_is_rejected_before_anything_is_exchanged() {
        let flow = FlowState { state: "expected".into(), nonce: "n".into(), pkce_verifier: "v".into() };

        assert!(!callback_state_matches(&flow, "attacker-supplied"));
        assert!(callback_state_matches(&flow, "expected"));
    }

    #[test]
    fn a_provider_error_is_surfaced_rather_than_swallowed() {
        let params = CallbackParams {
            code: None,
            state: None,
            error: Some("access_denied".into()),
        };

        assert_eq!(classify_callback(&params), CallbackOutcome::ProviderError("access_denied".into()));
    }

    #[test]
    fn a_callback_without_a_code_is_malformed() {
        let params = CallbackParams { code: None, state: Some("st".into()), error: None };

        assert_eq!(classify_callback(&params), CallbackOutcome::Malformed);
    }
```

`classify_callback` and `callback_state_matches` are small pure helpers so the callback's decision table is testable without a provider. `CallbackOutcome` needs `PartialEq` and `Debug`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms auth_api`
Expected: FAIL — `cannot find function flow_cookie`.

- [ ] **Step 3: Implement the handlers**

- `login` — generate `state`, `nonce` and a PKCE verifier from `rand`, set the flow cookie, 302 to `provider.authorize_url(...)`.
- `callback` — `classify_callback`, then read the flow cookie, compare `state`, exchange the code, `resolve_or_provision`, `create_session`, set the session cookie, clear the flow cookie, 302 to `/`. `ClaimRejected` is a 403 with no session and no principal created.
- `logout` — revoke the session behind the cookie, return `clear_cookie_header`. Local only: the provider's session is left alone, so the user stays signed in to their other applications. RP-initiated logout is explicitly not implemented.
- `me` — read `Extension<AuthContext>`, return the principal plus its granted portfolios, reusing the query behind `handlers::list_portfolios`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms auth_api`
Expected: PASS, 6 new tests plus Task 9's five under `--ignored`.

- [ ] **Step 5: Commit**

```bash
git add src/auth_api.rs
git commit -m "feat(auth): login, callback, logout, and who-am-I"
```

---

### Task 11: The audit trail learns who acted

**Files:**
- Modify: `src/handlers.rs:550`, `src/handlers.rs:713`, `src/handlers.rs:982`
- Test: in-module `#[cfg(test)]` in `src/handlers.rs`

**Interfaces:**
- Consumes: `AuthContext { principal_code }` from Task 5.
- Produces: `fn actor_for(auth: &AuthContext) -> String`

`EventMetadata.actor` (`src/domain/orders/aggregate.rs:11`) currently receives `"oms"` for anything the OMS decided. That discards identity the request already carried — and the per-order timeline shipped on 2026-09-15 surfaces the field directly, so the gap is now visible to users.

**This changes existing behaviour** for token-authenticated commands, not only sessions. It is intended.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_authenticated_command_is_attributed_to_whoever_sent_it() {
        let auth = AuthContext {
            principal_id: Uuid::nil(),
            principal_code: "jane.doe".to_string(),
        };

        assert_eq!(actor_for(&auth), "jane.doe");
    }
```

Leave `src/execution.rs:155` alone: broker-driven events keep the broker's name, and events the OMS generates on its own — expiry sweeps, reconciliation — keep `"oms"`. That split is the rule; this test pins the half that changes.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin oms an_authenticated_command`
Expected: FAIL — `cannot find function actor_for`.

- [ ] **Step 3: Implement**

```rust
/// Who to record as having caused an event.
///
/// `"oms"` is reserved for events the system generates on its own — expiry
/// sweeps, reconciliation. A command that arrived with a credential is
/// attributed to that credential's principal, whether it came from a browser
/// session or an API key.
fn actor_for(auth: &AuthContext) -> String {
    auth.principal_code.clone()
}
```

Then replace the hardcoded actor at each of the three `EventMetadata` construction sites in `handlers.rs` with `actor_for(&auth)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, whole suite.

- [ ] **Step 5: Verify against a real order**

Submit an order, then `oms orders history <id>` (added 2026-09-15). The `actor` column should show the principal's code rather than `oms` for the submitted and routed events, and the broker's name for any fill.

- [ ] **Step 6: Commit**

```bash
git add src/handlers.rs
git commit -m "feat(audit): attribute an order event to the principal that caused it"
```

---

### Task 12: Admin session revocation and the expiry sweep

**Files:**
- Modify: `src/admin.rs`, `src/main.rs`
- Test: in-module `#[cfg(test)]` in `src/admin.rs`, `#[ignore]`d

**Interfaces:**
- Consumes: `revoke_all_for_principal`, `sweep_expired` (Task 4).
- Produces: `pub async fn revoke_principal_sessions(...) -> Result<Json<RevokedSessions>, AdminError>` on `DELETE /admin/principals/:id/sessions`, and `pub struct RevokedSessions { pub revoked: u64 }`

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn revoking_sessions_reports_how_many_it_killed() {
        let pool = test_pool().await;
        let principal_id = seed_principal(&pool).await;
        crate::sessions::create_session(&pool, principal_id, &Default::default(), None)
            .await
            .expect("session");

        let revoked = crate::sessions::revoke_all_for_principal(&pool, principal_id)
            .await
            .expect("revoke");

        assert_eq!(revoked, 1);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin oms admin -- --ignored`
Expected: FAIL to compile, or fail on the missing helper, depending on what exists.

- [ ] **Step 3: Implement**

Add the handler, following the shape of `revoke_trading_token` (`src/admin.rs:2070`): log the action, call the store, return the count. Register a periodic `sweep_expired` task in `main.rs` next to the other background spawns, running hourly and logging the number removed.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms admin -- --ignored`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/admin.rs src/main.rs
git commit -m "feat(admin): revoke a principal's sessions, and sweep expired ones"
```

---

### Task 13: Wiring, OpenAPI, sample config, and the end-to-end profile

**Files:**
- Modify: `src/main.rs` (router, `ApiDoc`), `docs/openapi.json`, `docker-compose.yml`, `oms.toml` sample in `src/config.rs:249`, `README.md`
- Test: the existing drift test at `src/main.rs:1346`

**Interfaces:**
- Consumes: every handler from Tasks 10 and 12.
- Produces: the mounted routes.

- [ ] **Step 1: Mount the routes**

A fourth router, unauthenticated by definition, merged only when `config.oidc()` is `Some`:

```rust
let auth_router = Router::new()
    .route("/auth/login", get(auth_api::login))
    .route("/auth/callback", get(auth_api::callback))
    .route("/auth/logout", post(auth_api::logout));
```

`/auth/me` belongs on `orders_router` instead — it requires authentication, so it goes behind `auth_middleware` with everything else. When `config.oidc()` is `None`, do not merge `auth_router` at all: `/auth/*` then falls through to `handler_404`, which is the desired off-by-default behaviour.

Add `admin::revoke_principal_sessions` to `admin_router`.

- [ ] **Step 2: Register in `ApiDoc`**

Add `auth_api::login`, `auth_api::callback`, `auth_api::logout`, `auth_api::me` and `admin::revoke_principal_sessions` to `paths(...)`, and `auth_api::MeResponse` plus `admin::RevokedSessions` to `components(schemas(...))`.

- [ ] **Step 3: Refuse an unsafe deployment**

Where the bind address is resolved in `main.rs` (near the admin-password rule at line 803), add: if `config.oidc()` is `Some` and the bind is not loopback and the public base URL is not `https://`, log an error and `std::process::exit(1)`. The cookie cannot carry `Secure` in that configuration, so the session would travel in clear text.

- [ ] **Step 4: Regenerate the spec**

Run: `cargo run -- openapi > docs/openapi.json`
Run: `cargo test`
Expected: PASS, including `the_committed_openapi_spec_is_current`.

- [ ] **Step 5: Document it**

Add a commented `[auth.oidc]` block to the sample config in `src/config.rs:249`, noting that the client secret is set with `oms config` and never written to the file. Add a short README section: what to register at the provider (the redirect URI is `{public_base_url}/auth/callback`), and that omitting the block leaves the OMS exactly as it is today.

- [ ] **Step 6: Add the end-to-end profile**

Add a Keycloak service to `docker-compose.yml` under a non-default profile, with a realm import creating one client and one user. Document the two commands to bring it up and run a real login. Keep it out of CI: it exists to be run deliberately.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/config.rs docs/openapi.json docker-compose.yml README.md
git commit -m "feat(auth): mount the OIDC routes, off unless configured"
```

---

## Self-Review

**Spec coverage.** Session storage → Task 4. Token hashing rationale → Tasks 1, 4. Lifetime and revocation → Tasks 1, 4, 12. Cookie attributes and the loopback rule → Tasks 2, 13. CSRF → Task 3. Endpoints → Task 10. The flow, `state`/`nonce`/PKCE → Task 10. Client registration and the sealed client secret → Tasks 8, 13. Discovery and JWKS → Task 8. Token handling (nothing persisted) → Task 8. Validation seam → Task 7. Principal resolution → Task 9. JIT provisioning and the claim gate → Task 9. Authorization unchanged → Task 5. Audit/actor → Task 11. Config and first-run → Tasks 6, 13. Failure-mode table → Tasks 7, 9, 10. Testing → every task.

**Two gaps found and closed while reviewing:** the CSRF check from Task 3 needs to be *applied*, not merely defined — it belongs in `auth_middleware` alongside the session branch in Task 5, and the implementer should wire it there. And `sweep_expired` was specified in Task 4 but only scheduled in Task 12; that ordering is intentional but worth naming.

**Type consistency.** `AuthContext { principal_id, principal_code }` is used identically in Tasks 5, 10 and 11. `SessionToken { plaintext, hash }` in Tasks 1, 4 and 5. `VerifiedIdentity { subject, display_name, email, claims }` in Tasks 7, 8 and 9. `SessionTtl { idle, absolute }` in Tasks 1, 4, 5 and 6. `verify_key` returns `(Uuid, String)` from Task 5 onwards, and its one caller is updated in the same task.

**Known risk.** Task 7 is the task to slow down on. If `openidconnect`'s verifier does not expose the error granularity those eight tests assert, keep the tests and adapt the error mapping — do not weaken an assertion to fit the library.
