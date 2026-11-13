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
    /// How this request authenticated; decides whether `mfa` matters.
    pub auth_method: AuthMethod,
    /// The session passed MFA: a local session's TOTP, or an OIDC login whose
    /// `amr` met `[auth.oidc].require_amr` (evaluated once, at login).
    pub mfa: bool,
}

/// How a request authenticated, as far as the MFA gate cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    Oidc,
    Local,
    /// The deprecated shared admin password (or a session minted from it).
    LegacyAdmin,
    /// A machine credential: an API key, or a strategy's own session.
    ApiKey,
    /// The per-boot host token (`oms` CLI on the host), a machine credential.
    HostCli,
}

impl AuthContext {
    /// A machine identity (API key, strategy): exempt from the MFA gate.
    pub fn machine(principal_id: Uuid, principal_code: String) -> Self {
        AuthContext { principal_id, principal_code, auth_method: AuthMethod::ApiKey, mfa: false }
    }

    fn from_session(record: crate::sessions::SessionRecord) -> Self {
        let auth_method = match record.auth_method {
            crate::sessions::AuthMethod::Oidc => AuthMethod::Oidc,
            crate::sessions::AuthMethod::Local => AuthMethod::Local,
            crate::sessions::AuthMethod::LegacyAdmin => AuthMethod::LegacyAdmin,
        };
        AuthContext {
            principal_id: record.principal_id,
            principal_code: record.principal_code,
            auth_method,
            mfa: record.mfa_verified_at.is_some(),
        }
    }
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
    if let Some(record) = authenticate_session(pool, headers, session_config).await? {
        return Ok(Some((AuthContext::from_session(record), CredentialKind::Session)));
    }

    let Ok((key_id, secret)) = extract_trading_credentials(headers) else {
        return Ok(None);
    };
    Ok(verify_key(pool, &key_id, &secret).await?.map(|(principal_id, principal_code)| {
        (AuthContext::machine(principal_id, principal_code), CredentialKind::ApiKey)
    }))
}

/// The session half of `authenticate`: the live session record behind the
/// request's cookie (ACTIVE HUMAN principal, not revoked, within its TTLs), or
/// `None`. Exposed whole so `admin_middleware` can see how the session was
/// established and whether it passed MFA.
pub async fn authenticate_session(
    pool: &sqlx::PgPool,
    headers: &axum::http::HeaderMap,
    session_config: &SessionConfig,
) -> Result<Option<crate::sessions::SessionRecord>, Response> {
    let policy = &session_config.cookie_policy;
    let Some(value) = crate::sessions::cookie_from_headers(headers, crate::sessions::cookie_name(policy)) else {
        return Ok(None);
    };
    let hash = crate::sessions::hash_session_token(&value);
    let Some(record) = crate::sessions::lookup_session(pool, &hash).await.map_err(|_| service_unavailable())? else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    if crate::sessions::is_expired(now, record.last_seen_at, record.absolute_expires_at, &session_config.ttl) {
        return Ok(None);
    }
    if crate::sessions::needs_touch(now, record.last_seen_at) {
        crate::sessions::touch_session(pool, record.id).await.map_err(|_| service_unavailable())?;
    }
    Ok(Some(record))
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

    if let Some(code) = crate::strategies::stopped_strategy(&state.strategies().all(), ctx.principal_id) {
        return Err(axum::response::IntoResponse::into_response((StatusCode::FORBIDDEN, format!("strategy {code} is stopped"))));
    }

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

/// Guard for `/admin/*`.
///
/// The normal door is a session (cookie) of an ACTIVE principal holding a
/// `principal_admin` row. A state-changing request on it must pass the CSRF
/// origin check and, for a mutation, `authz::require_mfa`.
/// API keys never open `/admin/*`.
///
/// The legacy door: while `system_setting.legacy_admin_enabled` is true, the
/// deprecated shared password is also accepted as `Bearer <password>`. It
/// acts as the synthetic `legacy-admin`, which may read any admin route and
/// create a named admin (`POST /admin/users/bootstrap-admin`), nothing else.
/// Its `principal_id` is the nil UUID, which never exists in `principal`, so
/// it can never satisfy a trading or authz check.
///
/// The host door: `Bearer <host token>` (written 0600 next to the config at
/// boot) from a loopback peer acts as `host-cli` with full admin capability,
/// before and after cutover. It is how `oms status` / `oms setup sync-broker`
/// reach the server.
pub async fn admin_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let pool = state.pool();
    if let Some(session) = authenticate_session(pool, req.headers(), &state.session_config).await? {
        enforce_origin_for_session(CredentialKind::Session, req.headers(), req.method(), &state.session_config)?;
        let is_admin = crate::admin_bootstrap::is_active_admin(pool, session.principal_id)
            .await
            .map_err(|_| service_unavailable())?;
        if !is_admin {
            return Err(forbidden_because("admin access required"));
        }
        let ctx = AuthContext::from_session(session);
        admin_mfa_gate(req.method(), &ctx).map_err(axum::response::IntoResponse::into_response)?;
        req.extensions_mut().insert(ctx);
        return Ok(next.run(req).await);
    }

    let token = extract_bearer_token(req.headers()).map_err(|_| unauthorized())?;
    // The per-boot host token: loopback peers only, independent of the
    // legacy flag; acts as `host-cli`.
    let peer = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    if crate::admin_bootstrap::host_token_ok(state.host_token_hash.as_deref(), &token, peer) {
        let ctx = crate::admin_bootstrap::host_context();
        admin_mfa_gate(req.method(), &ctx).map_err(axum::response::IntoResponse::into_response)?;
        req.extensions_mut().insert(ctx);
        return Ok(next.run(req).await);
    }
    let Some(password) = state.legacy_admin_password.as_deref() else {
        return Err(unauthorized());
    };
    if !constant_time_eq(token.as_bytes(), password.as_bytes()) {
        return Err(unauthorized());
    }
    let enabled = crate::admin_bootstrap::legacy_admin_enabled(pool).await.map_err(|_| service_unavailable())?;
    if !enabled {
        return Err(forbidden_because("the shared admin password is disabled; sign in as a named admin"));
    }
    if !crate::admin_bootstrap::legacy_may(req.method(), req.uri().path()) {
        return Err(forbidden_because("create a named admin first"));
    }
    // Not passed through `admin_mfa_gate`: `legacy_may` already confines it to
    // reads and the first-admin bootstrap, which must work before anyone has MFA.
    req.extensions_mut().insert(crate::admin_bootstrap::legacy_context());
    Ok(next.run(req).await)
}

/// `/admin/*` mutations need MFA (`authz::require_mfa`); reads never do.
pub fn admin_mfa_gate(method: &axum::http::Method, ctx: &AuthContext) -> Result<(), crate::handlers::ApiError> {
    if crate::sessions::is_read_method(method) {
        return Ok(());
    }
    crate::authz::require_mfa(ctx)
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

fn forbidden_because(reason: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(reason.into())
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
            crate::sessions::AuthMethod::Oidc,
            false,
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
            crate::sessions::AuthMethod::Oidc,
            false,
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
