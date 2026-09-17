use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::sessions::SessionConfig;

#[derive(Clone)]
pub struct AuthContext {
    pub principal_id: Uuid,
    /// The acting principal's `code`, carried so a handler can stamp an
    /// event's `actor` without a second query.
    pub principal_code: String,
}

/// Which door a request came through. Authorization never branches on this —
/// a session and an API key carrying the same `principal_id` have identical
/// powers — the only thing that differs by kind is the CSRF origin check in
/// `auth_middleware`, which applies to `Session` only: the existing Python
/// client and `oms` CLI send a Bearer token and no `Origin` header at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Session,
    ApiKey,
}

/// Resolve whichever credential the request carries.
///
/// Session cookie first, key material second. Both produce the same
/// `AuthContext`: the difference between a human and a machine ends here, and
/// no handler downstream can tell them apart.
///
/// `session_config` is threaded in rather than derived from a bind address
/// here, so the cookie policy is computed once at boot (see `AppState`) and
/// so the idle/absolute TTL is never silently hardcoded — a later task makes
/// it configurable, and doing the derivation per call would ignore that.
pub async fn authenticate(
    pool: &sqlx::PgPool,
    headers: &axum::http::HeaderMap,
    session_config: &SessionConfig,
) -> Result<Option<(AuthContext, CredentialKind)>, Response> {
    let policy = &session_config.cookie_policy;
    if let Some(value) =
        crate::sessions::cookie_from_headers(headers, crate::sessions::cookie_name(policy))
    {
        let hash = crate::sessions::hash_session_token(&value);
        if let Some(record) = crate::sessions::lookup_session(pool, &hash)
            .await
            .map_err(|_| service_unavailable())?
        {
            let now = chrono::Utc::now();
            if crate::sessions::is_expired(
                now,
                record.last_seen_at,
                record.absolute_expires_at,
                &session_config.ttl,
            ) {
                return Ok(None);
            }
            if crate::sessions::needs_touch(now, record.last_seen_at) {
                crate::sessions::touch_session(pool, record.id)
                    .await
                    .map_err(|_| service_unavailable())?;
            }
            return Ok(Some((
                AuthContext {
                    principal_id: record.principal_id,
                    principal_code: record.principal_code,
                },
                CredentialKind::Session,
            )));
        }
    }

    let Ok((key_id, secret)) = extract_trading_credentials(headers) else {
        return Ok(None);
    };
    Ok(verify_key(pool, &key_id, &secret).await?.map(|(principal_id, principal_code)| {
        (AuthContext { principal_id, principal_code }, CredentialKind::ApiKey)
    }))
}

/// Authenticate a trading request and inject `AuthContext`.
///
/// Accepts two doors that both resolve to the same `AuthContext`:
/// - A session cookie, set at sign-in (see `sessions.rs`).
/// - An API key, in one of two equivalent credential forms carrying the same
///   `(key_id, secret)`:
///   - HTTP **Basic** `key_id:secret` — the original form, kept for back-compat.
///   - **Bearer** `key_id.secret` — a single copy-paste "trading token"
///     (Databento style). Split on the first `.` (neither `ak_…` key ids nor
///     `sk_…` secrets contain a dot).
///
/// A session-authenticated request additionally has to pass the CSRF origin
/// check (`origin_is_allowed`) — an API-key request never carries an `Origin`
/// header at all, so gating those on it would break every existing caller.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let (ctx, kind) = authenticate(state.pool(), req.headers(), &state.session_config)
        .await?
        .ok_or_else(unauthorized)?;

    enforce_origin_for_session(kind, req.headers(), req.method(), &state.session_config)?;

    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

/// The CSRF origin check, applied to `Session`-authenticated requests only —
/// a no-op for `ApiKey`, which never carries an `Origin` header at all (the
/// existing Python client and `oms` CLI would fail every request if gated on
/// it).
///
/// The expected origin must be a *configured* value (`session_config`'s
/// `public_base_url`), never derived from this request's own `Host` header —
/// a `Host`-derived expectation moves with whatever `Host` an attacker sends
/// (DNS rebinding, a proxy forwarding an attacker-controlled `Host`), so the
/// check would always pass.
///
/// A missing `public_base_url` is refused rather than falling back to
/// anything derived from the request — but only for state-changing methods:
/// a read changes nothing, so blocking it buys no security, only
/// availability loss (`sessions::is_read_method` is the same list
/// `origin_is_allowed` itself exempts, so the two can't drift apart). In
/// practice the state-changing branch is unreachable, since a session can
/// only ever be minted by the OIDC callback, and OIDC configuration always
/// carries `public_base_url` alongside it. If it's missing here anyway, fail
/// closed on writes.
fn enforce_origin_for_session(
    kind: CredentialKind,
    headers: &axum::http::HeaderMap,
    method: &axum::http::Method,
    session_config: &SessionConfig,
) -> Result<(), Response> {
    if kind != CredentialKind::Session || crate::sessions::is_read_method(method) {
        return Ok(());
    }
    let Some(public_base_url) = session_config.public_base_url.as_deref() else {
        return Err(forbidden());
    };
    if !crate::sessions::origin_is_allowed(headers, method, public_base_url) {
        return Err(forbidden());
    }
    Ok(())
}

/// Look up an active api key by `key_id` and bcrypt-verify `secret`. Returns
/// the owning `(principal_id, principal_code)` on success, `None` if the key
/// is unknown/revoked or the secret doesn't match. `Err` only for
/// infrastructure failures (DB / task join).
pub async fn verify_key(
    pool: &sqlx::PgPool,
    key_id: &str,
    secret: &str,
) -> Result<Option<(Uuid, String)>, Response> {
    let row = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT k.principal_id, p.code, k.secret_hash
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        WHERE k.key_id = $1 AND k.revoked_at IS NULL AND p.status = 'ACTIVE'
        "#,
    )
    .bind(key_id)
    .fetch_optional(pool)
    .await
    .map_err(|_| service_unavailable())?;

    let Some((principal_id, principal_code, secret_hash)) = row else {
        return Ok(None);
    };

    // bcrypt is CPU-bound — run it off the async thread pool
    let secret = secret.to_string();
    let valid = tokio::task::spawn_blocking(move || bcrypt::verify(&secret, &secret_hash))
        .await
        .map_err(|_| service_unavailable())?
        .map_err(|_| unauthorized())?;

    Ok(valid.then_some((principal_id, principal_code)))
}

pub async fn admin_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    if !state.admin_auth_enabled {
        return Ok(next.run(req).await);
    }

    let token = extract_bearer_token(req.headers()).map_err(|_| forbidden())?;

    if !constant_time_eq(token.as_bytes(), state.admin_token.as_bytes()) {
        return Err(forbidden());
    }

    Ok(next.run(req).await)
}

/// Extract `(key_id, secret)` from either credential form:
/// - `Authorization: Basic base64(key_id:secret)`
/// - `Authorization: Bearer key_id.secret` (single trading token)
fn extract_trading_credentials(headers: &header::HeaderMap) -> Result<(String, String), Response> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(unauthorized)?;

    if let Some(encoded) = value.strip_prefix("Basic ") {
        let decoded = STANDARD.decode(encoded).map_err(|_| unauthorized())?;
        let credentials = String::from_utf8(decoded).map_err(|_| unauthorized())?;
        let (key_id, secret) = credentials.split_once(':').ok_or_else(unauthorized)?;
        if key_id.is_empty() || secret.is_empty() {
            return Err(unauthorized());
        }
        return Ok((key_id.to_string(), secret.to_string()));
    }

    if let Some(token) = value.strip_prefix("Bearer ") {
        let (key_id, secret) = token.split_once('.').ok_or_else(unauthorized)?;
        if key_id.is_empty() || secret.is_empty() {
            return Err(unauthorized());
        }
        return Ok((key_id.to_string(), secret.to_string()));
    }

    Err(unauthorized())
}

fn extract_bearer_token(headers: &header::HeaderMap) -> Result<String, ()> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(())?;

    let token = value.strip_prefix("Bearer ").ok_or(())?;

    if token.is_empty() {
        return Err(());
    }

    Ok(token.to_string())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body("unauthorized".into())
        .unwrap()
}

fn forbidden() -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body("forbidden".into())
        .unwrap()
}

fn service_unavailable() -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body("service unavailable".into())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{cookie_policy, SessionTtl};

    fn session_config() -> SessionConfig {
        SessionConfig {
            cookie_policy: cookie_policy("localhost:3001", None),
            ttl: SessionTtl::default(),
            public_base_url: None,
        }
    }

    #[test]
    fn a_session_authenticated_state_change_is_refused_when_no_base_url_is_configured() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("origin", "https://oms.example.com".parse().unwrap());

        let result = enforce_origin_for_session(
            CredentialKind::Session,
            &headers,
            &axum::http::Method::POST,
            &session_config(), // public_base_url: None
        );

        assert!(result.is_err(), "no configured base URL must fail closed, not fall back to anything request-derived");
    }

    #[test]
    fn a_session_authenticated_read_is_allowed_even_when_no_base_url_is_configured() {
        // A GET changes nothing, so refusing it for want of a configured
        // base URL buys no security — only availability loss. The fail-closed
        // rule in `enforce_origin_for_session` must apply to writes only.
        let result = enforce_origin_for_session(
            CredentialKind::Session,
            &axum::http::HeaderMap::new(),
            &axum::http::Method::GET,
            &session_config(), // public_base_url: None
        );

        assert!(result.is_ok(), "reads must not be blocked by a missing public_base_url");
    }

    #[test]
    fn a_session_authenticated_state_change_is_allowed_when_the_origin_matches_the_configured_base_url() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("origin", "https://oms.example.com".parse().unwrap());
        let mut config = session_config();
        config.public_base_url = Some("https://oms.example.com".to_string());

        assert!(enforce_origin_for_session(
            CredentialKind::Session,
            &headers,
            &axum::http::Method::POST,
            &config,
        )
        .is_ok());
    }

    #[test]
    fn a_session_authenticated_state_change_is_refused_when_the_origin_does_not_match() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("origin", "https://evil.example.com".parse().unwrap());
        let mut config = session_config();
        config.public_base_url = Some("https://oms.example.com".to_string());

        assert!(enforce_origin_for_session(
            CredentialKind::Session,
            &headers,
            &axum::http::Method::POST,
            &config,
        )
        .is_err());
    }

    #[test]
    fn an_api_key_request_is_never_gated_on_origin_even_with_no_base_url_configured() {
        // The existing Python client and `oms` CLI send a Bearer token and no
        // Origin header at all — this must never be refused on that basis.
        assert!(enforce_origin_for_session(
            CredentialKind::ApiKey,
            &axum::http::HeaderMap::new(),
            &axum::http::Method::POST,
            &session_config(), // public_base_url: None
        )
        .is_ok());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_session_cookie_authenticates_exactly_like_a_key() {
        let pool = test_pool().await;
        let (principal_id, code) = seed_principal(&pool, "auth-session-test").await;
        let token = crate::sessions::create_session(
            &pool,
            principal_id,
            &SessionTtl::default(),
            None,
        )
        .await
        .expect("create session");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", format!("oms_session={}", token.plaintext).parse().unwrap());

        let (ctx, kind) = authenticate(&pool, &headers, &session_config())
            .await
            .expect("authenticate")
            .expect("a session should authenticate");

        assert_eq!(ctx.principal_id, principal_id);
        assert_eq!(ctx.principal_code, code, "the code must ride along, for the audit trail");
        assert_eq!(kind, CredentialKind::Session);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_idle_session_no_longer_authenticates() {
        let pool = test_pool().await;
        let (principal_id, _) = seed_principal(&pool, "auth-idle-test").await;
        let token = crate::sessions::create_session(
            &pool,
            principal_id,
            &SessionTtl::default(),
            None,
        )
        .await
        .expect("create session");

        // Backdate past the 30-minute idle window.
        sqlx::query(
            "UPDATE user_session SET last_seen_at = now() - interval '31 minutes' \
             WHERE token_hash = $1",
        )
        .bind(&token.hash)
        .execute(&pool)
        .await
        .expect("backdate");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("cookie", format!("oms_session={}", token.plaintext).parse().unwrap());

        assert!(authenticate(&pool, &headers, &session_config())
            .await
            .expect("authenticate")
            .is_none());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn no_credential_of_either_kind_is_not_an_authentication() {
        let pool = test_pool().await;

        assert!(authenticate(&pool, &axum::http::HeaderMap::new(), &session_config())
            .await
            .expect("authenticate")
            .is_none());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_api_key_still_authenticates_and_is_tagged_as_such() {
        let pool = test_pool().await;
        let (principal_id, code) = seed_principal(&pool, "auth-apikey-test").await;
        let key_id = format!("ak_test_auth_{}", Uuid::new_v4());
        let secret = "s3cr3t-value";
        let secret_hash = bcrypt::hash(secret, bcrypt::DEFAULT_COST).expect("hash");
        sqlx::query(
            "INSERT INTO api_key (key_id, principal_id, secret_hash) VALUES ($1, $2, $3)",
        )
        .bind(&key_id)
        .bind(principal_id)
        .bind(&secret_hash)
        .execute(&pool)
        .await
        .expect("seed api key");

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {key_id}.{secret}").parse().unwrap(),
        );

        let (ctx, kind) = authenticate(&pool, &headers, &session_config())
            .await
            .expect("authenticate")
            .expect("an api key should authenticate");

        assert_eq!(ctx.principal_id, principal_id);
        assert_eq!(ctx.principal_code, code);
        assert_eq!(kind, CredentialKind::ApiKey);
    }

    // ── test plumbing ────────────────────────────────────────────────────────
    // Copied from `sessions.rs`'s test module — see its comments for why.

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
