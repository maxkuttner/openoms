//! Local sign-in: sign-up (the first account becomes the admin), email +
//! password login, password change, TOTP (enrol, confirm, verify, disable,
//! recovery codes), and logout-all.
//!
//! Every route here answers with `Cache-Control: no-store` and
//! `Referrer-Policy: no-referrer` (see `no_store`), and every POST is
//! Origin-checked against the configured public base URL — including the
//! pre-session ones, so a cross-site page cannot log a victim into an
//! attacker's account (login CSRF).
//!
//! Failures are deliberately uniform: an unknown email, a wrong password and
//! a locked account all answer `{"error":"invalid_credentials"}` after one
//! Argon2 verify, so neither the body nor the timing tells them apart.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, Extension, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::local_auth;
use crate::secrets::MasterKey;
use crate::sessions::{self, AuthMethod, SessionRecord};

/// The issuer recorded in `principal_identity` for a local account; the
/// subject is the principal id.
pub const LOCAL_ISSUER: &str = "urn:openoms:local";
const TOTP_ISSUER: &str = "OpenOMS";
pub const ACCOUNT_FAILURES_PER_WINDOW: u32 = 5;
pub const IP_FAILURES_PER_WINDOW: u32 = 20;
const LIMIT_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Consecutive wrong passwords before `local_credential.locked_until` is set.
const LOCKOUT_AFTER: i32 = 10;
/// Upper bound on tracked limiter keys, so the limiter itself is not a
/// memory DoS. Expired entries are evicted first; if the map is still full,
/// new keys are treated as blocked (fail closed).
const LIMITER_MAX_KEYS: usize = 100_000;

// ── rate limiting ──────────────────────────────────────────────────────────

/// In-memory fixed-window failure counters, keyed by account and by client IP.
pub struct LoginLimiter {
    account_limit: u32,
    ip_limit: u32,
    window: Duration,
    buckets: Mutex<HashMap<String, (Instant, u32)>>,
}

impl LoginLimiter {
    pub fn new(account_limit: u32, ip_limit: u32, window: Duration) -> Self {
        Self { account_limit, ip_limit, window, buckets: Mutex::new(HashMap::new()) }
    }

    fn live(&self, entry: Option<&(Instant, u32)>, now: Instant) -> u32 {
        match entry {
            Some((start, n)) if now.duration_since(*start) < self.window => *n,
            _ => 0,
        }
    }

    /// Check and charge one attempt against both keys under a single lock,
    /// so concurrent attempts cannot all slip past the check before any of
    /// them is counted. `false` means over the limit (nothing is charged).
    /// The caller refunds the charge on success (`refund`) and keeps it on
    /// failure.
    pub fn try_acquire(&self, account: &str, ip: &str) -> bool {
        let now = Instant::now();
        let keys = [format!("a:{account}"), format!("i:{ip}")];
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if self.live(map.get(&keys[0]), now) >= self.account_limit || self.live(map.get(&keys[1]), now) >= self.ip_limit {
            return false;
        }
        let new_keys = keys.iter().filter(|k| !map.contains_key(*k)).count();
        if map.len() + new_keys > LIMITER_MAX_KEYS {
            let window = self.window;
            map.retain(|_, (start, _)| now.duration_since(*start) < window);
            if map.len() + new_keys > LIMITER_MAX_KEYS {
                return false; // fail closed rather than grow without bound
            }
        }
        for key in keys {
            let entry = map.entry(key).or_insert((now, 0));
            if now.duration_since(entry.0) >= self.window {
                *entry = (now, 0);
            }
            entry.1 += 1;
        }
        true
    }

    /// Return the charge of an attempt that succeeded.
    pub fn refund(&self, account: &str, ip: &str) {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        for key in [format!("a:{account}"), format!("i:{ip}")] {
            if let Some(entry) = map.get_mut(&key) {
                entry.1 = entry.1.saturating_sub(1);
            }
        }
    }

    /// Forget an account's failures (a successful login).
    pub fn clear_account(&self, account: &str) {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner()).remove(&format!("a:{account}"));
    }
}

/// What the local routes need beyond `AppState`, handed over as an
/// `Extension` built once at boot (see `main.rs`'s `build_app`).
#[derive(Clone)]
pub struct LocalAuth {
    pub limiter: Arc<LoginLimiter>,
    /// TOTP secrets are sealed with it; without one, TOTP is unavailable.
    pub master_key: Option<Arc<MasterKey>>,
    pub local_enabled: bool,
}

impl LocalAuth {
    pub fn new(master_key: Option<MasterKey>, local_enabled: bool) -> Self {
        Self {
            limiter: Arc::new(LoginLimiter::new(ACCOUNT_FAILURES_PER_WINDOW, IP_FAILURES_PER_WINDOW, LIMIT_WINDOW)),
            master_key: master_key.map(Arc::new),
            local_enabled,
        }
    }
}

// ── response plumbing ──────────────────────────────────────────────────────

/// Response layer for every route in this module.
pub async fn no_store(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    res
}

fn fail(status: StatusCode, error: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

fn internal(what: &str, err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "{what} failed");
    fail(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

fn invalid_credentials() -> Response {
    fail(StatusCode::UNAUTHORIZED, "invalid_credentials")
}

fn with_cookie(mut res: Response, cookie: String) -> Response {
    res.headers_mut().append(header::SET_COOKIE, cookie.parse().expect("cookie header is plain ASCII"));
    res
}

fn session_cookie(state: &AppState, token: &str) -> String {
    let cfg = &state.session_config;
    sessions::set_cookie_header(&cfg.cookie_policy, token, cfg.ttl.absolute)
}

/// CSRF: a POST must carry our own `Origin`. Fails closed with no configured
/// base URL.
fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    match state.session_config.public_base_url.as_deref() {
        Some(url) if sessions::origin_is_allowed(headers, &Method::POST, url) => Ok(()),
        _ => Err(Box::new(fail(StatusCode::FORBIDDEN, "forbidden"))),
    }
}

/// The live, unexpired session behind the request's cookie, regardless of
/// MFA state (these routes are where MFA is earned).
async fn current_session(state: &AppState, headers: &HeaderMap) -> Result<SessionRecord, Response> {
    let unauth = || fail(StatusCode::UNAUTHORIZED, "unauthenticated");
    let policy = &state.session_config.cookie_policy;
    let value = sessions::cookie_from_headers(headers, sessions::cookie_name(policy)).ok_or_else(unauth)?;
    let record = sessions::lookup_session(state.pool(), &sessions::hash_session_token(&value))
        .await
        .map_err(|e| internal("lookup_session", e))?
        .ok_or_else(unauth)?;
    if sessions::is_expired(Utc::now(), record.last_seen_at, record.absolute_expires_at, &state.session_config.ttl) {
        return Err(unauth());
    }
    Ok(record)
}

/// The current local session of an approved account: a PENDING one answers
/// 403 `approval_pending`.
async fn local_session(state: &AppState, headers: &HeaderMap) -> Result<SessionRecord, Response> {
    let s = current_session(state, headers).await?;
    if s.pending {
        return Err(crate::auth::approval_pending());
    }
    if s.auth_method != AuthMethod::Local {
        return Err(fail(StatusCode::BAD_REQUEST, "not_a_local_session"));
    }
    Ok(s)
}

fn master(la: &LocalAuth) -> Result<&MasterKey, Box<Response>> {
    la.master_key.as_deref().ok_or_else(|| Box::new(fail(StatusCode::SERVICE_UNAVAILABLE, "mfa_unavailable")))
}

fn client_ip(conn: &Option<ConnectInfo<SocketAddr>>) -> String {
    conn.as_ref().map(|c| c.0.ip().to_string()).unwrap_or_else(|| "unknown".into())
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
}

/// One Argon2 verify against a fixed dummy hash, so a refusal that never
/// reaches a real hash costs the same as one that does.
async fn burn_verify(password: &str) {
    static DUMMY: OnceCell<String> = OnceCell::const_new();
    let hash = DUMMY
        .get_or_init(|| async { local_auth::hash_password("timing-equaliser-not-a-password").await.unwrap_or_default() })
        .await;
    let _ = local_auth::verify_password(hash, password).await;
}

/// Revoke whatever session the request already carried: pre-auth session
/// material must not survive a new sign-in.
async fn drop_prior_session(state: &AppState, headers: &HeaderMap) {
    if let Ok(prior) = current_session(state, headers).await {
        if sessions::revoke_session(state.pool(), prior.id).await.is_ok() {
            state.event_bus().publish_auth_changed(prior.principal_id);
        }
    }
}

/// Count one failed password or TOTP attempt in the database; the 10th
/// consecutive one locks the credential for 15 minutes. One atomic UPDATE,
/// so the lockout holds across processes and concurrent requests.
async fn record_db_failure(pool: &PgPool, principal_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE local_credential SET \
            failed_attempts = CASE WHEN failed_attempts + 1 >= $2 THEN 0 ELSE failed_attempts + 1 END, \
            locked_until = CASE WHEN failed_attempts + 1 >= $2 THEN now() + interval '15 minutes' \
                                ELSE locked_until END \
          WHERE principal_id = $1 \
         RETURNING COALESCE(locked_until > now(), false)",
    )
    .bind(principal_id)
    .bind(LOCKOUT_AFTER)
    .fetch_one(pool)
    .await
}

// ── sign-up ────────────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
pub struct NextStep {
    /// `totp`, `totp_enroll` (only with `[auth.local].require_mfa`) or `done`.
    pub next: &'static str,
}

/// What a local session must do next, given its credential and the policy.
fn next_step(has_totp: bool, mfa_verified: bool, require_mfa: bool) -> &'static str {
    if has_totp && !mfa_verified {
        "totp"
    } else if require_mfa && !has_totp {
        "totp_enroll"
    } else {
        "done"
    }
}

/// Whether a fresh password sign-in already satisfies the MFA gate: only
/// when MFA is optional and the user has no TOTP to ask for.
fn password_is_enough(require_mfa: bool, has_totp: bool) -> bool {
    !require_mfa && !has_totp
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SignupBody {
    pub name: String,
    pub email: String,
    pub password: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SignupResponse {
    /// `admin` (the first account), `user` or `pending`.
    pub role: &'static str,
    /// As on login: `totp_enroll` (only with `require_mfa`) or `done`.
    pub next: &'static str,
}

/// Create a local account and sign it in. The first account on a fresh
/// install becomes the admin; later ones need `[auth].enable_signup` (403
/// `signup_disabled` otherwise) and get `[auth].default_user_role`. The
/// password policy applies. Rate-limited like login.
#[utoipa::path(
    post, path = "/auth/local/signup", tag = "auth",
    request_body = SignupBody,
    responses(
        (status = 200, body = SignupResponse, description = "Created; session cookie set"),
        (status = 400, description = "weak_password, invalid_email or email_taken"),
        (status = 403, description = "signup_disabled, or a cross-origin request"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn signup(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<SignupBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let account = format!("signup:{}", body.email.trim().to_lowercase());
    let ip = client_ip(&conn);
    if !la.limiter.try_acquire(&account, &ip) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    if let Err(reason) = local_auth::check_password_policy(&body.password) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "weak_password", "reason": reason })))
            .into_response();
    }
    let hash = match local_auth::hash_password(&body.password).await {
        Ok(h) => h,
        Err(e) => return internal("hash_password", e),
    };
    use crate::accounts::{AccountError, AccountRole};
    let created = match crate::accounts::signup(state.pool(), &body.name, &body.email, &hash, state.signup).await {
        Ok(c) => c,
        Err(AccountError::SignupDisabled) => return fail(StatusCode::FORBIDDEN, "signup_disabled"),
        Err(AccountError::EmailTaken) => return fail(StatusCode::BAD_REQUEST, "email_taken"),
        Err(AccountError::BadEmail) => return fail(StatusCode::BAD_REQUEST, "invalid_email"),
        Err(AccountError::Db(e)) => return internal("signup", e),
    };
    la.limiter.refund(&account, &ip);
    drop_prior_session(&state, &headers).await;
    let mfa = password_is_enough(state.require_mfa, false);
    let token = match sessions::create_session(
        state.pool(), created.principal_id, AuthMethod::Local, mfa, &state.session_config.ttl, user_agent(&headers),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return internal("create_session", e),
    };
    // A pending account can do nothing yet, TOTP enrolment included.
    let next = if created.role == AccountRole::Pending { "done" } else { next_step(false, mfa, state.require_mfa) };
    let res = Json(SignupResponse { role: created.role.as_str(), next }).into_response();
    with_cookie(res, session_cookie(&state, &token.plaintext))
}

// ── login ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

/// Email + password. Success mints a
/// session and says what comes next (see `NextStep`). MFA is pending when the
/// user has TOTP or `[auth.local].require_mfa` is on. Every failure is the same generic 401; over the
/// rate limit it is a generic 429.
#[utoipa::path(
    post, path = "/auth/local/login", tag = "auth", operation_id = "local_login",
    request_body = LoginBody,
    responses(
        (status = 200, body = NextStep, description = "Password accepted; session cookie set"),
        (status = 401, description = "invalid_credentials"),
        (status = 403, description = "Cross-origin request"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn login(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let account = body.email.trim().to_lowercase();
    let ip = client_ip(&conn);
    if !la.limiter.try_acquire(&account, &ip) {
        burn_verify("x").await;
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    // Over-long input never reaches a real hash; the charge is kept.
    if body.password.chars().count() > local_auth::MAX_PASSWORD_CHARS {
        burn_verify("x").await;
        return invalid_credentials();
    }

    let row = sqlx::query(
        "SELECT p.id, c.password_hash, c.totp_enrolled_at IS NOT NULL AS has_totp, \
                COALESCE(c.locked_until > now(), false) AS locked \
           FROM principal p JOIN local_credential c ON c.principal_id = p.id \
          WHERE lower(p.email) = $1 AND p.status IN ('ACTIVE', 'PENDING') AND p.principal_type = 'HUMAN' \
          LIMIT 1",
    )
    .bind(&account)
    .fetch_optional(state.pool())
    .await;
    let row = match row {
        Ok(r) => r,
        Err(e) => return internal("local login lookup", e),
    };
    let Some(row) = row else {
        burn_verify(&body.password).await;
        return invalid_credentials();
    };
    let principal_id: Uuid = row.get("id");
    let hash: String = row.get("password_hash");
    let ok = local_auth::verify_password(&hash, &body.password).await;
    if row.get::<bool, _>("locked") {
        return invalid_credentials();
    }
    if !ok {
        if let Err(e) = record_db_failure(state.pool(), principal_id).await {
            return internal("record failed login", e);
        }
        return invalid_credentials();
    }
    la.limiter.refund(&account, &ip);
    la.limiter.clear_account(&account);

    if let Err(e) = sqlx::query("UPDATE local_credential SET failed_attempts = 0, locked_until = NULL WHERE principal_id = $1")
        .bind(principal_id)
        .execute(state.pool())
        .await
    {
        return internal("reset failed logins", e);
    }
    drop_prior_session(&state, &headers).await;
    let has_totp: bool = row.get("has_totp");
    let mfa = password_is_enough(state.require_mfa, has_totp);
    let token = match sessions::create_session(
        state.pool(), principal_id, AuthMethod::Local, mfa, &state.session_config.ttl, user_agent(&headers),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return internal("create_session", e),
    };
    let next = next_step(has_totp, mfa, state.require_mfa);
    with_cookie(Json(NextStep { next }).into_response(), session_cookie(&state, &token.plaintext))
}

// ── TOTP ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CodeBody {
    /// A 6-digit TOTP code, or a recovery code.
    pub code: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct MfaVerified {
    pub mfa_verified: bool,
}

/// Rotate `session` to a fresh MFA-verified one and set its cookie on `res`.
async fn rotate_verified(state: &AppState, session: &SessionRecord, headers: &HeaderMap, res: Response) -> Response {
    match sessions::rotate_session(
        state.pool(),
        session.id,
        session.principal_id,
        AuthMethod::Local,
        true,
        &state.session_config.ttl,
        user_agent(headers),
    )
    .await
    {
        Ok(Some(token)) => {
            // the old session is revoked: its open sockets close now, not at the next tick
            state.event_bus().publish_auth_changed(session.principal_id);
            with_cookie(res, session_cookie(state, &token.plaintext))
        }
        Ok(None) => fail(StatusCode::UNAUTHORIZED, "unauthenticated"),
        Err(e) => internal("rotate_session", e),
    }
}

fn looks_like_totp(code: &str) -> bool {
    let c = code.trim();
    c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit())
}

/// Second factor on the current local session: a TOTP code or an unused
/// recovery code. Success sets `mfa_verified_at` by rotating the session —
/// the old token is revoked in the same transaction.
#[utoipa::path(
    post, path = "/auth/local/totp", tag = "auth",
    request_body = CodeBody,
    responses(
        (status = 200, body = MfaVerified, description = "Verified; rotated session cookie set"),
        (status = 401, description = "Wrong code or no session"),
        (status = 409, description = "TOTP not enrolled yet"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn totp(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match local_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if session.mfa_verified_at.is_some() {
        // Already verified: nothing to earn, and no needless rotation.
        return Json(MfaVerified { mfa_verified: true }).into_response();
    }
    let account = format!("p:{}", session.principal_id);
    let ip = client_ip(&conn);
    let master = match master(&la) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    if !la.limiter.try_acquire(&account, &ip) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    let row = sqlx::query(
        "SELECT totp_secret_enc, totp_last_step, COALESCE(locked_until > now(), false) AS locked \
           FROM local_credential WHERE principal_id = $1 AND totp_enrolled_at IS NOT NULL",
    )
    .bind(session.principal_id)
    .fetch_optional(state.pool())
    .await;
    let row = match row {
        Ok(Some(r)) => r,
        Ok(None) => return fail(StatusCode::CONFLICT, "totp_not_enrolled"),
        Err(e) => return internal("load credential", e),
    };

    let accepted = if row.get::<bool, _>("locked") {
        Ok(false)
    } else if looks_like_totp(&body.code) {
        let secret: Vec<u8> = row.get("totp_secret_enc");
        match local_auth::verify_totp(&secret, master, &body.code, row.get("totp_last_step"), Utc::now().timestamp()) {
            // Conditional on the stored step, so two concurrent requests with
            // the same code cannot both pass.
            Ok(step) => sqlx::query(
                "UPDATE local_credential SET totp_last_step = $2 \
                  WHERE principal_id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)",
            )
            .bind(session.principal_id)
            .bind(step)
            .execute(state.pool())
            .await
            .map(|r| r.rows_affected() == 1),
            Err(_) => Ok(false),
        }
    } else {
        use_recovery_code(state.pool(), session.principal_id, &body.code).await
    };
    match accepted {
        Ok(true) => {
            la.limiter.refund(&account, &ip);
            if let Err(e) = clear_failures(state.pool(), session.principal_id).await {
                return internal("clear failures", e);
            }
            rotate_verified(&state, &session, &headers, Json(MfaVerified { mfa_verified: true }).into_response()).await
        }
        Ok(false) => match record_db_failure(state.pool(), session.principal_id).await {
            Ok(_) => fail(StatusCode::UNAUTHORIZED, "invalid_code"),
            Err(e) => internal("record failed totp", e),
        },
        Err(e) => internal("verify second factor", e),
    }
}

/// A passed second factor resets the failure count and any expired lock.
async fn clear_failures(pool: &PgPool, principal_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE local_credential SET failed_attempts = 0, locked_until = NULL WHERE principal_id = $1")
        .bind(principal_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Spend one unused recovery code, atomically: only the request whose update
/// flips `used_at` succeeds.
async fn use_recovery_code(pool: &PgPool, principal_id: Uuid, code: &str) -> Result<bool, sqlx::Error> {
    let hashes: Vec<String> =
        sqlx::query_scalar("SELECT code_hash FROM recovery_code WHERE principal_id = $1 AND used_at IS NULL")
            .bind(principal_id)
            .fetch_all(pool)
            .await?;
    let Some(hash) = hashes.into_iter().find(|h| local_auth::verify_recovery_code(code, h)) else {
        return Ok(false);
    };
    let spent = sqlx::query(
        "UPDATE recovery_code SET used_at = now() WHERE principal_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(principal_id)
    .bind(hash)
    .execute(pool)
    .await?;
    Ok(spent.rows_affected() == 1)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct TotpEnrollResponse {
    pub otpauth_uri: String,
}

/// Start TOTP enrolment on the current local session: a fresh secret is
/// stored (sealed) as pending and its `otpauth://` URI returned. Calling it
/// again before confirming replaces the pending secret.
#[utoipa::path(
    post, path = "/auth/local/totp/enroll", tag = "auth",
    responses(
        (status = 200, body = TotpEnrollResponse),
        (status = 401, description = "No session"),
        (status = 409, description = "TOTP already enrolled"),
    ),
)]
pub async fn totp_enroll(State(state): State<AppState>, Extension(la): Extension<LocalAuth>, headers: HeaderMap) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match local_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let master = match master(&la) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    let email: Option<String> = match sqlx::query_scalar("SELECT email FROM principal WHERE id = $1")
        .bind(session.principal_id)
        .fetch_one(state.pool())
        .await
    {
        Ok(e) => e,
        Err(e) => return internal("load principal", e),
    };
    // An email with a `:` cannot be an otpauth label; fall back to the code.
    let enrollment = match email.as_deref().map(|e| local_auth::new_totp(TOTP_ISSUER, e, master)) {
        Some(Ok(en)) => en,
        _ => match local_auth::new_totp(TOTP_ISSUER, &session.principal_code, master) {
            Ok(en) => en,
            Err(e) => return internal("new_totp", e),
        },
    };
    let stored = sqlx::query(
        "UPDATE local_credential SET totp_secret_enc = $2, totp_last_step = NULL \
          WHERE principal_id = $1 AND totp_enrolled_at IS NULL",
    )
    .bind(session.principal_id)
    .bind(&enrollment.secret_enc)
    .execute(state.pool())
    .await;
    match stored {
        Ok(r) if r.rows_affected() == 1 => Json(TotpEnrollResponse { otpauth_uri: enrollment.otpauth_uri.clone() }).into_response(),
        Ok(_) => fail(StatusCode::CONFLICT, "totp_already_enrolled"),
        Err(e) => internal("store pending totp", e),
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct RecoveryCodes {
    /// Shown exactly once; only hashes are stored.
    pub recovery_codes: Vec<String>,
}

/// Activate the pending TOTP secret with a valid code. Issues the recovery
/// codes (returned once) and rotates the session to an MFA-verified one.
#[utoipa::path(
    post, path = "/auth/local/totp/confirm", tag = "auth",
    request_body = CodeBody,
    responses(
        (status = 200, body = RecoveryCodes, description = "TOTP active; rotated session cookie set"),
        (status = 401, description = "Wrong code or no session"),
        (status = 409, description = "No pending enrolment"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn totp_confirm(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match local_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let account = format!("p:{}", session.principal_id);
    let ip = client_ip(&conn);
    let master = match master(&la) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    if !la.limiter.try_acquire(&account, &ip) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    let pending: Option<(Vec<u8>, bool)> = match sqlx::query_as(
        "SELECT totp_secret_enc, COALESCE(locked_until > now(), false) FROM local_credential \
          WHERE principal_id = $1 AND totp_enrolled_at IS NULL AND totp_secret_enc IS NOT NULL",
    )
    .bind(session.principal_id)
    .fetch_optional(state.pool())
    .await
    {
        Ok(s) => s,
        Err(e) => return internal("load pending totp", e),
    };
    let Some((secret, locked)) = pending else {
        return fail(StatusCode::CONFLICT, "no_pending_totp");
    };
    // A locked account fails like a wrong code, as on `totp` and `login`.
    let verified = if locked { None } else { local_auth::verify_totp(&secret, master, &body.code, None, Utc::now().timestamp()).ok() };
    let Some(step) = verified else {
        if let Err(e) = record_db_failure(state.pool(), session.principal_id).await {
            return internal("record failed totp", e);
        }
        return fail(StatusCode::UNAUTHORIZED, "invalid_code");
    };
    la.limiter.refund(&account, &ip);
    let (shown, hashes) = local_auth::new_recovery_codes();
    match activate_totp(state.pool(), session.principal_id, &session.principal_code, step, &hashes, &secret).await {
        Ok(true) => {}
        Ok(false) => return fail(StatusCode::CONFLICT, "no_pending_totp"),
        Err(e) => return internal("activate totp", e),
    }
    rotate_verified(&state, &session, &headers, Json(RecoveryCodes { recovery_codes: shown }).into_response()).await
}

/// Activates only `verified_secret` — the secret the code was checked
/// against — so a concurrent re-enrol that swapped in a new pending secret
/// cannot get an unproven secret activated.
async fn activate_totp(
    pool: &PgPool,
    principal_id: Uuid,
    code: &str,
    step: i64,
    hashes: &[String],
    verified_secret: &[u8],
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let activated = sqlx::query(
        "UPDATE local_credential SET totp_enrolled_at = now(), totp_last_step = $2, \
                failed_attempts = 0, locked_until = NULL \
          WHERE principal_id = $1 AND totp_enrolled_at IS NULL AND totp_secret_enc = $3 \
            AND (locked_until IS NULL OR locked_until <= now())",
    )
    .bind(principal_id)
    .bind(step)
    .bind(verified_secret)
    .execute(&mut *tx)
    .await?;
    if activated.rows_affected() != 1 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM recovery_code WHERE principal_id = $1")
        .bind(principal_id)
        .execute(&mut *tx)
        .await?;
    for hash in hashes {
        sqlx::query("INSERT INTO recovery_code (principal_id, code_hash) VALUES ($1, $2)")
            .bind(principal_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
    }
    crate::accounts::audit(&mut tx, code, "local.totp_enrolled", principal_id, serde_json::json!({})).await?;
    tx.commit().await?;
    Ok(true)
}

/// Turn TOTP off for the current local session's user. Needs a valid current
/// 6-digit code; clears the secret and the recovery codes; audited
/// `local.totp_disabled`. Refused (409 `mfa_required_by_policy`) while
/// `[auth.local].require_mfa` is on.
#[utoipa::path(
    post, path = "/auth/local/totp/disable", tag = "auth",
    request_body = CodeBody,
    responses(
        (status = 204, description = "TOTP removed"),
        (status = 401, description = "Wrong code or no session"),
        (status = 409, description = "Not enrolled, or MFA is required by policy"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn totp_disable(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<CodeBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match local_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if state.require_mfa {
        return fail(StatusCode::CONFLICT, "mfa_required_by_policy");
    }
    let master = match master(&la) {
        Ok(m) => m,
        Err(r) => return *r,
    };
    let account = format!("p:{}", session.principal_id);
    let ip = client_ip(&conn);
    if !la.limiter.try_acquire(&account, &ip) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    let row: Option<(Vec<u8>, Option<i64>, bool)> = match sqlx::query_as(
        "SELECT totp_secret_enc, totp_last_step, COALESCE(locked_until > now(), false) \
           FROM local_credential WHERE principal_id = $1 AND totp_enrolled_at IS NOT NULL",
    )
    .bind(session.principal_id)
    .fetch_optional(state.pool())
    .await
    {
        Ok(r) => r,
        Err(e) => return internal("load credential", e),
    };
    let Some((secret, last_step, locked)) = row else {
        return fail(StatusCode::CONFLICT, "totp_not_enrolled");
    };
    let valid = !locked
        && looks_like_totp(&body.code)
        && local_auth::verify_totp(&secret, master, &body.code, last_step, Utc::now().timestamp()).is_ok();
    if !valid {
        return match record_db_failure(state.pool(), session.principal_id).await {
            Ok(_) => fail(StatusCode::UNAUTHORIZED, "invalid_code"),
            Err(e) => internal("record failed totp", e),
        };
    }
    la.limiter.refund(&account, &ip);
    match remove_totp(state.pool(), session.principal_id, &session.principal_code, &secret).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => fail(StatusCode::CONFLICT, "totp_not_enrolled"),
        Err(e) => internal("disable totp", e),
    }
}

/// Removes only `verified_secret`, the one the code was checked against.
async fn remove_totp(pool: &PgPool, principal_id: Uuid, code: &str, verified_secret: &[u8]) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query(
        "UPDATE local_credential SET totp_secret_enc = NULL, totp_enrolled_at = NULL, totp_last_step = NULL, \
                failed_attempts = 0, locked_until = NULL \
          WHERE principal_id = $1 AND totp_enrolled_at IS NOT NULL AND totp_secret_enc = $2",
    )
    .bind(principal_id)
    .bind(verified_secret)
    .execute(&mut *tx)
    .await?;
    if removed.rows_affected() != 1 {
        return Ok(false);
    }
    sqlx::query("DELETE FROM recovery_code WHERE principal_id = $1").bind(principal_id).execute(&mut *tx).await?;
    crate::accounts::audit(&mut tx, code, "local.totp_disabled", principal_id, serde_json::json!({})).await?;
    tx.commit().await?;
    Ok(true)
}

// ── password change ────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChangePasswordBody {
    pub current_password: String,
    pub new_password: String,
}

/// Change the current local session's password. The new one must pass the
/// password policy (15 to 128 characters, not a common password) and differ
/// from the current one. Rotates the session (the old token stops at once;
/// MFA state carries over) and says what comes next (`totp`, `totp_enroll` or
/// `done`). Audited `local.password_changed`.
#[utoipa::path(
    post, path = "/auth/local/password", tag = "auth",
    request_body = ChangePasswordBody,
    responses(
        (status = 200, body = NextStep, description = "Changed; rotated session cookie set"),
        (status = 400, description = "weak_password or same_password"),
        (status = 401, description = "Wrong current password, or no session"),
        (status = 429, description = "too_many_attempts"),
    ),
)]
pub async fn change_password(
    State(state): State<AppState>,
    Extension(la): Extension<LocalAuth>,
    conn: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match local_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let account = format!("p:{}", session.principal_id);
    let ip = client_ip(&conn);
    if !la.limiter.try_acquire(&account, &ip) {
        return fail(StatusCode::TOO_MANY_REQUESTS, "too_many_attempts");
    }
    let row: (String, bool, bool) = match sqlx::query_as(
        "SELECT password_hash, totp_enrolled_at IS NOT NULL, COALESCE(locked_until > now(), false) \
           FROM local_credential WHERE principal_id = $1",
    )
    .bind(session.principal_id)
    .fetch_one(state.pool())
    .await
    {
        Ok(r) => r,
        Err(e) => return internal("load credential", e),
    };
    let (hash, has_totp, locked) = row;
    let ok = body.current_password.chars().count() <= local_auth::MAX_PASSWORD_CHARS
        && local_auth::verify_password(&hash, &body.current_password).await;
    if locked || !ok {
        if let Err(e) = record_db_failure(state.pool(), session.principal_id).await {
            return internal("record failed password", e);
        }
        return invalid_credentials();
    }
    la.limiter.refund(&account, &ip);
    if let Err(reason) = local_auth::check_password_policy(&body.new_password) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "weak_password", "reason": reason })))
            .into_response();
    }
    if body.new_password == body.current_password {
        return fail(StatusCode::BAD_REQUEST, "same_password");
    }
    let new_hash = match local_auth::hash_password(&body.new_password).await {
        Ok(h) => h,
        Err(e) => return internal("hash_password", e),
    };
    if let Err(e) = store_password(state.pool(), session.principal_id, &session.principal_code, &new_hash).await {
        return internal("store password", e);
    }
    let mfa = session.mfa_verified_at.is_some();
    let next = next_step(has_totp, mfa, state.require_mfa);
    match sessions::rotate_session(
        state.pool(),
        session.id,
        session.principal_id,
        AuthMethod::Local,
        mfa,
        &state.session_config.ttl,
        user_agent(&headers),
    )
    .await
    {
        Ok(Some(token)) => {
            state.event_bus().publish_auth_changed(session.principal_id);
            with_cookie(Json(NextStep { next }).into_response(), session_cookie(&state, &token.plaintext))
        }
        Ok(None) => fail(StatusCode::UNAUTHORIZED, "unauthenticated"),
        Err(e) => internal("rotate_session", e),
    }
}

async fn store_password(pool: &PgPool, principal_id: Uuid, code: &str, hash: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE local_credential SET password_hash = $2, \
                failed_attempts = 0, locked_until = NULL WHERE principal_id = $1",
    )
    .bind(principal_id)
    .bind(hash)
    .execute(&mut *tx)
    .await?;
    crate::accounts::audit(&mut tx, code, "local.password_changed", principal_id, serde_json::json!({})).await?;
    tx.commit().await
}

// ── sign-in methods ───────────────────────────────────────────────────────

/// The sign-in methods the login page may offer. Unauthenticated.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuthMethods {
    /// Email + password (then TOTP, if the user has it).
    pub local: bool,
    /// The configured OIDC provider, reachable at boot.
    pub sso: bool,
    pub sso_label: &'static str,
    /// The sign-up form may be shown (`[auth].enable_signup`, local on).
    pub signup: bool,
    /// No admin exists yet: the page offers "Create the admin account".
    pub first_run: bool,
}

#[derive(Clone, Copy)]
pub struct EnabledMethods {
    local: bool,
    sso: bool,
}

/// Which sign-in buttons to show. Only enabled methods are reported; a
/// database error reports `first_run` false (fail closed).
#[utoipa::path(
    get, path = "/auth/methods", tag = "auth",
    responses((status = 200, body = AuthMethods)),
)]
pub async fn methods(State(state): State<AppState>, Extension(m): Extension<EnabledMethods>) -> Json<AuthMethods> {
    let first_run = m.local && crate::accounts::first_run(state.pool()).await.unwrap_or(false);
    Json(AuthMethods {
        local: m.local,
        sso: m.sso,
        sso_label: "Sign in with SSO",
        signup: m.local && state.signup.enabled,
        first_run,
    })
}

/// `GET /auth/methods`, mounted whatever is configured: with nothing on it
/// says so, and the login page shows that instead of a dead button.
pub fn methods_routes(local: bool, sso: bool) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/auth/methods", axum::routing::get(methods))
        .layer(Extension(EnabledMethods { local, sso }))
        .layer(axum::middleware::map_response(no_store))
}

// ── router ─────────────────────────────────────────────────────────────────

/// The routes of this module. Logout-all serves any sign-in method; the
/// `/auth/local/*` routes only exist when local login is enabled.
pub fn routes(la: LocalAuth) -> axum::Router<AppState> {
    use axum::routing::post;
    let mut router = axum::Router::new().route("/auth/logout-all", post(logout_all));
    if la.local_enabled {
        router = router
            .route("/auth/local/signup", post(signup))
            .route("/auth/local/login", post(login))
            .route("/auth/local/totp", post(totp))
            .route("/auth/local/totp/enroll", post(totp_enroll))
            .route("/auth/local/totp/confirm", post(totp_confirm))
            .route("/auth/local/totp/disable", post(totp_disable))
            .route("/auth/local/password", post(change_password));
    }
    router.layer(Extension(la)).layer(axum::middleware::map_response(no_store))
}

// ── logout-all ─────────────────────────────────────────────────────────────

/// Revoke every session of the signed-in user, this one included, and clear
/// the cookie.
#[utoipa::path(
    post, path = "/auth/logout-all", tag = "auth",
    responses(
        (status = 204, description = "All sessions revoked; cookie cleared"),
        (status = 401, description = "No session"),
        (status = 403, description = "Cross-origin request"),
    ),
)]
pub async fn logout_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = check_origin(&state, &headers) {
        return *r;
    }
    let session = match current_session(&state, &headers).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if session.pending {
        return crate::auth::approval_pending();
    }
    if let Err(e) = sessions::revoke_all_for_principal(state.pool(), session.principal_id).await {
        return internal("revoke_all_for_principal", e);
    }
    state.event_bus().publish_auth_changed(session.principal_id);
    with_cookie(StatusCode::NO_CONTENT.into_response(), sessions::clear_cookie_header(&state.session_config.cookie_policy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use axum::body::to_bytes;
    use std::net::{IpAddr, Ipv4Addr};

    const PASSWORD: &str = "correct horse battery staple";
    const ORIGIN: &str = "http://localhost:3001";

    fn key() -> crate::secrets::MasterKey {
        crate::secrets::parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
    }

    fn la_with(account: u32, ip: u32) -> LocalAuth {
        LocalAuth {
            limiter: Arc::new(LoginLimiter::new(account, ip, std::time::Duration::from_secs(900))),
            master_key: Some(Arc::new(key())),
            local_enabled: true,
        }
    }

    fn la() -> LocalAuth {
        la_with(ACCOUNT_FAILURES_PER_WINDOW, IP_FAILURES_PER_WINDOW)
    }

    /// Strict MFA (`require_mfa = true`), the mode these tests were written
    /// for; the optional default is covered in `first_run_tests`.
    fn app(pool: PgPool) -> AppState {
        let mut st = crate::handlers::tests::test_app_state(pool);
        st.session_config.public_base_url = Some(ORIGIN.to_string());
        st.require_mfa = true;
        st.signup = crate::accounts::SignupPolicy { enabled: true, pending_by_default: false };
        st
    }

    fn ip(n: u8) -> Option<ConnectInfo<SocketAddr>> {
        Some(ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, n)), 5000)))
    }

    fn hdrs(cookie: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("origin", ORIGIN.parse().unwrap());
        if let Some(c) = cookie {
            h.insert("cookie", format!("oms_session={c}").parse().unwrap());
        }
        h
    }

    fn cookie_of(resp: &Response) -> Option<String> {
        let raw = resp.headers().get(header::SET_COOKIE)?.to_str().ok()?;
        let value = raw.split(';').next()?.split_once('=')?.1;
        (!value.is_empty()).then(|| value.to_string())
    }

    async fn body(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        if bytes.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn signup_req(st: &AppState, la: &LocalAuth, email: &str, pw: &str) -> Response {
        signup(
            State(st.clone()),
            Extension(la.clone()),
            ip(1),
            hdrs(None),
            Json(SignupBody { name: "Test".into(), email: email.into(), password: pw.into() }),
        )
        .await
    }

    async fn login_req(st: &AppState, la: &LocalAuth, ipn: u8, email: &str, pw: &str) -> Response {
        login(
            State(st.clone()),
            Extension(la.clone()),
            ip(ipn),
            hdrs(None),
            Json(LoginBody { email: email.into(), password: pw.into() }),
        )
        .await
    }

    async fn totp_req(st: &AppState, la: &LocalAuth, cookie: &str, code: &str) -> Response {
        totp(State(st.clone()), Extension(la.clone()), ip(1), hdrs(Some(cookie)), Json(CodeBody { code: code.into() }))
            .await
    }

    fn code_at(uri: &str, offset: i64) -> String {
        let t = totp_rs::Totp::from_url(uri).unwrap();
        t.generate((chrono::Utc::now().timestamp() + offset) as u64).to_string()
    }

    /// Enrol `email` all the way: password, TOTP, confirm. Returns the
    /// otpauth URI, the recovery codes and the MFA-verified session cookie.
    async fn enrolled(st: &AppState, la: &LocalAuth, email: &str) -> (String, Vec<String>, String) {
        let resp = signup_req(st, la, email, PASSWORD).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = cookie_of(&resp).unwrap();
        let resp = totp_enroll(State(st.clone()), Extension(la.clone()), hdrs(Some(&cookie))).await;
        let uri = body(resp).await["otpauth_uri"].as_str().unwrap().to_string();
        let resp = totp_confirm(
            State(st.clone()),
            Extension(la.clone()),
            ip(1),
            hdrs(Some(&cookie)),
            Json(CodeBody { code: code_at(&uri, 0) }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = cookie_of(&resp).unwrap();
        let codes = body(resp).await["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        (uri, codes, cookie)
    }

    async fn session_of(pool: &PgPool, cookie: &str) -> Option<crate::sessions::SessionRecord> {
        crate::sessions::lookup_session(pool, &crate::sessions::hash_session_token(cookie)).await.unwrap()
    }

    /// Every route of the module, success or failure, carries the headers —
    /// here on a 500 (unreachable database) and a 403 (no Origin).
    #[tokio::test]
    async fn routes_set_no_store_and_no_referrer() {
        use tower::Service;
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let mut svc = routes(la()).with_state(app(pool));
        for (method, uri) in [("POST", "/auth/local/signup"), ("POST", "/auth/local/login"), ("POST", "/auth/logout-all")] {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"email":"a@b.c","password":"x"}"#))
                .unwrap();
            let resp = svc.call(req).await.unwrap();
            assert_ne!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(resp.headers()["cache-control"], "no-store", "{uri}");
            assert_eq!(resp.headers()["referrer-policy"], "no-referrer", "{uri}");
        }
    }

    async fn methods_body(pool: PgPool, local: bool, sso: bool) -> (StatusCode, serde_json::Value) {
        use tower::Service;
        let mut svc = methods_routes(local, sso).with_state(app(pool));
        let req = axum::http::Request::builder().uri("/auth/methods").body(axum::body::Body::empty()).unwrap();
        let resp = svc.call(req).await.unwrap();
        let status = resp.status();
        (status, body(resp).await)
    }

    /// Unauthenticated; reports the configured methods, without a database.
    #[tokio::test]
    async fn methods_reports_configured_methods() {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let (status, b) = methods_body(pool, true, false).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(b, serde_json::json!({ "local": true, "sso": false, "sso_label": "Sign in with SSO", "signup": true, "first_run": false }));
    }

    fn sso_only_settings() -> crate::config::AuthSettings {
        let cfg: crate::config::FileConfig =
            toml::from_str("[auth]\npolicy = \"sso_only\"\n[auth.local]\nenabled = true\n").unwrap();
        cfg.auth_settings()
    }

    /// `policy = "sso_only"` wins over `[auth.local].enabled = true`: the
    /// local login and enrol routes are not mounted.
    #[tokio::test]
    async fn sso_only_refuses_local_login_and_enrol() {
        use tower::Service;
        let s = sso_only_settings();
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let mut svc = routes(LocalAuth::new(Some(key()), s.local_enabled)).with_state(app(pool));
        for uri in ["/auth/local/login", "/auth/local/signup"] {
            let req = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("origin", ORIGIN)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"email":"a@b.c","password":"x","token":"t"}"#))
                .unwrap();
            let resp = svc.call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn methods_reports_no_local_under_sso_only() {
        let s = sso_only_settings();
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let (_, b) = methods_body(pool, s.local_enabled, true).await;
        assert_eq!(b["local"], false);
        assert_eq!(b["sso"], true);
    }

    #[test]
    fn next_step_follows_the_mfa_policy() {
        assert_eq!(next_step(true, false, false), "totp");
        assert_eq!(next_step(true, true, false), "done");
        assert_eq!(next_step(false, false, true), "totp_enroll");
        assert_eq!(next_step(false, true, false), "done");
        assert!(password_is_enough(false, false));
        assert!(!password_is_enough(false, true), "an enrolled TOTP is always asked for");
        assert!(!password_is_enough(true, false));
    }

    #[test]
    fn limiter_reserves_atomically_and_refunds() {
        let l = LoginLimiter::new(2, 3, std::time::Duration::from_secs(900));
        assert!(l.try_acquire("a", "1"));
        assert!(l.try_acquire("a", "1"));
        assert!(!l.try_acquire("a", "1"), "account limit");
        l.refund("a", "1");
        assert!(l.try_acquire("a", "1"), "a refunded success frees its slot");
        assert!(l.try_acquire("c", "1"));
        assert!(!l.try_acquire("d", "1"), "ip limit is shared across accounts");
        assert!(l.try_acquire("b", "2"));
        l.clear_account("a");
        assert!(l.try_acquire("a", "2"), "clearing an account resets its count");
        let short = LoginLimiter::new(1, 1, std::time::Duration::from_millis(0));
        assert!(short.try_acquire("a", "1"));
        assert!(short.try_acquire("a", "1"), "an expired window no longer counts");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn login_wrong_password_generic_error() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        enrolled(&st, &l, "dee@fund.test").await;
        let resp = login_req(&st, &l, 1, "dee@fund.test", "wrong wrong wrong wrong").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(cookie_of(&resp).is_none());
        assert_eq!(body(resp).await, serde_json::json!({ "error": "invalid_credentials" }));

        let ok = login_req(&st, &l, 1, "DEE@fund.test", PASSWORD).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let cookie = cookie_of(&ok).unwrap();
        assert_eq!(body(ok).await, serde_json::json!({ "next": "totp" }));
        assert!(session_of(&pool, &cookie).await.unwrap().mfa_verified_at.is_none());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn login_unknown_email_same_error() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let resp = login_req(&st, &la(), 1, "nobody@fund.test", PASSWORD).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body(resp).await, serde_json::json!({ "error": "invalid_credentials" }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn totp_success_rotates_session() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let (uri, _, _) = enrolled(&st, &l, "eve@fund.test").await;
        let login = login_req(&st, &l, 1, "eve@fund.test", PASSWORD).await;
        let old = cookie_of(&login).unwrap();

        let bad = totp_req(&st, &l, &old, "000000").await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

        let code = code_at(&uri, 30);
        let resp = totp_req(&st, &l, &old, &code).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let new = cookie_of(&resp).unwrap();
        assert_ne!(new, old);
        assert!(session_of(&pool, &old).await.is_none(), "the old token must stop working at once");
        let s = session_of(&pool, &new).await.unwrap();
        assert!(s.mfa_verified_at.is_some());
        assert_eq!(s.auth_method, crate::sessions::AuthMethod::Local);

        // The same code cannot be replayed on a fresh login.
        let again = cookie_of(&login_req(&st, &l, 1, "eve@fund.test", PASSWORD).await).unwrap();
        // The very same code string: recomputing it could cross a 30 s step.
        let replay = totp_req(&st, &l, &again, &code).await;
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn recovery_code_single_use() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let (_, codes, _) = enrolled(&st, &l, "fay@fund.test").await;
        assert_eq!(codes.len(), 10);
        let c1 = cookie_of(&login_req(&st, &l, 1, "fay@fund.test", PASSWORD).await).unwrap();
        assert_eq!(totp_req(&st, &l, &c1, &codes[0].to_lowercase()).await.status(), StatusCode::OK);
        let c2 = cookie_of(&login_req(&st, &l, 1, "fay@fund.test", PASSWORD).await).unwrap();
        assert_eq!(totp_req(&st, &l, &c2, &codes[0]).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(totp_req(&st, &l, &c2, &codes[1]).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rate_limit_blocks_after_5_account_failures() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        enrolled(&st, &l, "gus@fund.test").await;
        for n in 0..5 {
            let r = login_req(&st, &l, 10 + n, "Gus@fund.test", "nope nope nope nope").await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        // Even the right password, from a fresh IP, is now refused.
        let r = login_req(&st, &l, 99, "gus@fund.test", PASSWORD).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(cookie_of(&r).is_none());
        assert_eq!(body(r).await, serde_json::json!({ "error": "too_many_attempts" }));

        // Per IP: 20 failures across accounts block that address.
        for n in 0..20 {
            login_req(&st, &l, 200, &format!("x{n}@fund.test"), "nope nope nope nope").await;
        }
        let r = login_req(&st, &l, 200, "someone@fund.test", PASSWORD).await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn lockout_after_10_failures() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la_with(1000, 1000);
        enrolled(&st, &l, "hal@fund.test").await;
        for _ in 0..10 {
            login_req(&st, &l, 1, "hal@fund.test", "nope nope nope nope").await;
        }
        let locked: bool = sqlx::query_scalar(
            "SELECT locked_until > now() + interval '14 minutes' FROM local_credential c \
               JOIN principal p ON p.id = c.principal_id WHERE p.email = 'hal@fund.test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(locked);
        let r = login_req(&st, &l, 1, "hal@fund.test", PASSWORD).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "a locked account refuses even the right password");
        assert_eq!(body(r).await, serde_json::json!({ "error": "invalid_credentials" }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn logout_all_revokes_every_session() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let (_, _, c1) = enrolled(&st, &l, "jo@fund.test").await;
        let c2 = cookie_of(&login_req(&st, &l, 1, "jo@fund.test", PASSWORD).await).unwrap();
        let resp = logout_all(State(st.clone()), hdrs(Some(&c1))).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(session_of(&pool, &c1).await.is_none());
        assert!(session_of(&pool, &c2).await.is_none());

        let no_origin = logout_all(State(st), HeaderMap::new()).await;
        assert_eq!(no_origin.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn parallel_totp_attempts_cannot_exceed_limit() {
        use futures_util::future::join_all;
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        enrolled(&st, &l, "par@fund.test").await;
        let cookie = cookie_of(&login_req(&st, &l, 1, "par@fund.test", PASSWORD).await).unwrap();
        let attempts = (0..20).map(|_| totp_req(&st, &l, &cookie, "000000"));
        let through = join_all(attempts).await.iter().filter(|r| r.status() != StatusCode::TOO_MANY_REQUESTS).count();
        assert!(through <= 5, "{through} TOTP attempts got past the limiter");

        let logins = (0..20).map(|n| login_req(&st, &l, 50 + n, "par@fund.test", "nope nope nope nope"));
        let through = join_all(logins).await.iter().filter(|r| r.status() != StatusCode::TOO_MANY_REQUESTS).count();
        assert!(through <= 5, "{through} logins got past the limiter");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn activation_requires_the_verified_secret() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let token = "swap@fund.test".to_string();
        let cookie = cookie_of(&signup_req(&st, &l, &token, PASSWORD).await).unwrap();
        totp_enroll(State(st.clone()), Extension(l.clone()), hdrs(Some(&cookie))).await;
        let pid = session_of(&pool, &cookie).await.unwrap().principal_id;
        // The code was checked against some other secret than the one now pending.
        let activated = activate_totp(&pool, pid, "swap", 1, &[], b"not-the-stored-secret").await.unwrap();
        assert!(!activated);
        let enrolled: bool = sqlx::query_scalar("SELECT totp_enrolled_at IS NOT NULL FROM local_credential WHERE principal_id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!enrolled);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn totp_failures_lock_account() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la_with(1000, 1000);
        let (uri, _, _) = enrolled(&st, &l, "tl@fund.test").await;
        let cookie = cookie_of(&login_req(&st, &l, 1, "tl@fund.test", PASSWORD).await).unwrap();
        for _ in 0..10 {
            assert_eq!(totp_req(&st, &l, &cookie, "000000").await.status(), StatusCode::UNAUTHORIZED);
        }
        let locked: bool = sqlx::query_scalar(
            "SELECT locked_until > now() FROM local_credential c JOIN principal p ON p.id = c.principal_id \
              WHERE p.email = 'tl@fund.test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(locked);
        assert_eq!(totp_req(&st, &l, &cookie, &code_at(&uri, 30)).await.status(), StatusCode::UNAUTHORIZED, "locked refuses a right code");
        assert_eq!(login_req(&st, &l, 1, "tl@fund.test", PASSWORD).await.status(), StatusCode::UNAUTHORIZED);
    }

    async fn lock_state(pool: &PgPool, email: &str) -> (i32, Option<chrono::DateTime<Utc>>) {
        sqlx::query_as(
            "SELECT c.failed_attempts, c.locked_until FROM local_credential c JOIN principal p ON p.id = c.principal_id \
              WHERE p.email = $1",
        )
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn set_lock(pool: &PgPool, email: &str, failed: i32, locked_until: &str) {
        sqlx::query(&format!(
            "UPDATE local_credential SET failed_attempts = $2, locked_until = {locked_until} \
              WHERE principal_id = (SELECT id FROM principal WHERE email = $1)"
        ))
        .bind(email)
        .bind(failed)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A locked account cannot activate TOTP either, even with a right code.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn totp_confirm_refused_while_locked() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la_with(1000, 1000);
        let token = "cl@fund.test".to_string();
        let cookie = cookie_of(&signup_req(&st, &l, &token, PASSWORD).await).unwrap();
        let uri = body(totp_enroll(State(st.clone()), Extension(l.clone()), hdrs(Some(&cookie))).await).await["otpauth_uri"]
            .as_str()
            .unwrap()
            .to_string();
        set_lock(&pool, "cl@fund.test", 0, "now() + interval '15 minutes'").await;
        let resp =
            totp_confirm(State(st.clone()), Extension(l.clone()), ip(1), hdrs(Some(&cookie)), Json(CodeBody { code: code_at(&uri, 0) }))
                .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let enrolled: bool = sqlx::query_scalar(
            "SELECT totp_enrolled_at IS NOT NULL FROM local_credential c JOIN principal p ON p.id = c.principal_id \
              WHERE p.email = 'cl@fund.test'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!enrolled);
    }

    /// A successful TOTP confirm or sign-in clears the failure count and any
    /// (expired) lock.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn successful_totp_resets_failures() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la_with(1000, 1000);
        let token = "rs@fund.test".to_string();
        let cookie = cookie_of(&signup_req(&st, &l, &token, PASSWORD).await).unwrap();
        let uri = body(totp_enroll(State(st.clone()), Extension(l.clone()), hdrs(Some(&cookie))).await).await["otpauth_uri"]
            .as_str()
            .unwrap()
            .to_string();
        set_lock(&pool, "rs@fund.test", 4, "now() - interval '1 minute'").await;
        let resp =
            totp_confirm(State(st.clone()), Extension(l.clone()), ip(1), hdrs(Some(&cookie)), Json(CodeBody { code: code_at(&uri, 0) }))
                .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(lock_state(&pool, "rs@fund.test").await, (0, None), "confirm resets");

        let cookie = cookie_of(&login_req(&st, &l, 1, "rs@fund.test", PASSWORD).await).unwrap();
        set_lock(&pool, "rs@fund.test", 4, "now() - interval '1 minute'").await;
        assert_eq!(totp_req(&st, &l, &cookie, &code_at(&uri, 30)).await.status(), StatusCode::OK);
        assert_eq!(lock_state(&pool, "rs@fund.test").await, (0, None), "totp resets");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn overlong_password_is_generic_failure() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        enrolled(&st, &l, "long@fund.test").await;
        let r = login_req(&st, &l, 1, "long@fund.test", &"a".repeat(local_auth::MAX_PASSWORD_CHARS + 1)).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body(r).await, serde_json::json!({ "error": "invalid_credentials" }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn totp_on_verified_session_does_not_rotate() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let (_, _, cookie) = enrolled(&st, &l, "vv@fund.test").await;
        let r = totp_req(&st, &l, &cookie, "000000").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(cookie_of(&r).is_none(), "no new cookie");
        assert!(session_of(&pool, &cookie).await.is_some(), "the session is untouched");
    }

    /// Captures every tracing event emitted on this thread while `f` runs.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn no_secrets_in_responses_or_logs() {
        let pool = migrated_db().await;
        let st = app(pool.clone());
        let l = la();
        let cap = Capture::default();
        let writer = cap.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let token = "kim@fund.test".to_string();
        let mut bodies = String::new();
        let r = signup_req(&st, &l, &token, PASSWORD).await;
        let cookie = cookie_of(&r).unwrap();
        bodies.push_str(&body(r).await.to_string());
        let r = totp_enroll(State(st.clone()), Extension(l.clone()), hdrs(Some(&cookie))).await;
        let uri = body(r).await["otpauth_uri"].as_str().unwrap().to_string();
        let secret = uri.split("secret=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let r = totp_confirm(State(st.clone()), Extension(l.clone()), ip(1), hdrs(Some(&cookie)), Json(CodeBody { code: "123".into() })).await;
        bodies.push_str(&body(r).await.to_string());
        for pw in [PASSWORD, "wrong wrong wrong wrong"] {
            let r = login_req(&st, &l, 1, "kim@fund.test", pw).await;
            bodies.push_str(&body(r).await.to_string());
        }
        bodies.push_str(&body(signup_req(&st, &l, &token, PASSWORD).await).await.to_string());

        let logs = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
        for (what, needle) in [("password", PASSWORD), ("totp secret", secret.as_str()), ("session token", cookie.as_str())] {
            assert!(!bodies.contains(needle), "{what} leaked into a response body");
            assert!(!logs.contains(needle), "{what} leaked into the logs");
        }
    }
}
