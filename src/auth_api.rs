//! The `/auth/*` HTTP handlers: OIDC login, the invite link that starts one,
//! the callback that resolves the verified identity through
//! `invites::resolve_login`, logout and `/auth/me`. Sign-in is invite-only:
//! nothing here creates a user.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::GrantedPortfolio;
use crate::handlers::{self, ApiError};
use crate::invites::{self, ClaimOutcome};
use crate::oidc::{OidcError, Provider};
use crate::sessions::{self, CookiePolicy};

// ── HTTP layer: /auth/login, /auth/callback, /auth/logout, /auth/me ────────
//
// Mounted in `main.rs`'s `serve()`: `/auth/login`, `/auth/callback` and
// `/auth/logout` on their own unauthenticated router (merged only when
// `config.oidc()` is `Some`), `/auth/me` on `orders_router` behind
// `auth_middleware`. `AuthApiState` is constructed there too, from
// `[auth.oidc]` config plus `oidc::Provider::discover`.

/// What `login` and `callback` need beyond `AppState`: the configured
/// provider connection and the optional claim gate the callback checks
/// before resolving the login. Kept out of `AppState` because OIDC is optional and off by
/// default (see `FileConfig::oidc`); Task 13 constructs this once at boot,
/// only when `[auth.oidc]` is configured, and hands it to the router as an
/// `Extension`.
#[derive(Clone)]
pub struct AuthApiState {
    pub provider: Arc<Provider>,
    pub required_claim: Option<(String, String)>,
    /// `[auth.oidc].require_amr`: a login whose `amr` lacks any of these gets
    /// a session without MFA (reads only; see `authz::require_mfa`).
    pub require_amr: Vec<String>,
}

/// `state`, `nonce`, and the PKCE verifier for one pending login. Minted by
/// `login`, carried to the browser in a short-lived cookie (see
/// `flow_cookie`), and read back by `callback` — never persisted to a table,
/// since they are per-browser and worthless the moment the callback returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowState {
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: String,
    /// The invite link token this login arrived with, if any. It is the
    /// only way an unknown identity can get in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_token: Option<String>,
}

/// Validate a caller-supplied post-login destination.
///
/// Only a same-site relative path is allowed. Everything else falls back to
/// `/`. This is the whole security value of the feature: without it,
/// `/auth/login` is an open redirect on the endpoint that mints the session,
/// which is worth more to an attacker than most bugs in this system.
pub fn sanitize_return_to(raw: Option<&str>) -> String {
    const DEFAULT: &str = "/";

    let Some(candidate) = raw else { return DEFAULT.to_string() };

    // Must be a path, not a URL, and not scheme-relative.
    let starts_with_single_slash = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.starts_with("/\\");
    if !starts_with_single_slash {
        return DEFAULT.to_string();
    }

    // A control character can smuggle a header or confuse a proxy; a real path
    // never has one.
    if candidate.chars().any(|c| c.is_control()) {
        return DEFAULT.to_string();
    }

    candidate.to_string()
}

/// How long the flow cookie lives. Long enough to cover a human clicking
/// through an IdP login form; short enough that an abandoned flow cookie is
/// useless well before anyone could find and replay it.
const FLOW_COOKIE_MAX_AGE_SECONDS: i64 = 300;

/// Distinct from the session cookie's name (`sessions::cookie_name`) so the
/// two can never collide or be confused for one another; `__Host-`-prefixed
/// under the same secure policy as the session cookie for the same reason
/// (see `sessions::cookie_name`).
fn flow_cookie_name(policy: &CookiePolicy) -> &'static str {
    if policy.secure {
        "__Host-oms_login_flow"
    } else {
        "oms_login_flow"
    }
}

/// Builds the `Set-Cookie` header that carries `flow` to the browser.
/// `HttpOnly` so no script on the page can read the PKCE verifier;
/// `SameSite=Lax` and `Max-Age=300` so it cannot outlive the login it was
/// minted for.
pub fn flow_cookie(policy: &CookiePolicy, flow: &FlowState) -> String {
    let encoded = encode_flow(flow);
    let mut header = format!(
        "{}={encoded}; Path=/; HttpOnly; SameSite=Lax; Max-Age={FLOW_COOKIE_MAX_AGE_SECONDS}",
        flow_cookie_name(policy)
    );
    if policy.secure {
        header.push_str("; Secure");
    }
    header
}

/// Expires the flow cookie immediately — used once `callback` has read it,
/// so a completed (or abandoned) login flow leaves nothing behind to replay.
fn clear_flow_cookie(policy: &CookiePolicy) -> String {
    let mut header = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        flow_cookie_name(policy)
    );
    if policy.secure {
        header.push_str("; Secure");
    }
    header
}

/// Reads `flow_cookie`'s value back out, or `None` if it is absent, expired,
/// or corrupt. A callback with no flow cookie cannot be trusted — there is
/// nothing to compare its `state` against — so this is the gate `callback`
/// checks before anything else touches the network.
pub fn flow_from_cookie(headers: &HeaderMap, policy: &CookiePolicy) -> Option<FlowState> {
    let value = sessions::cookie_from_headers(headers, flow_cookie_name(policy))?;
    decode_flow(&value)
}

/// JSON, then base64url (no padding) — the same alphabet already used for
/// `state`/`nonce`/`pkce_verifier` (see `random_token`), so the cookie value
/// never needs percent-encoding.
fn encode_flow(flow: &FlowState) -> String {
    let json = serde_json::to_vec(flow).expect("FlowState is plain strings; serialization cannot fail");
    URL_SAFE_NO_PAD.encode(json)
}

fn decode_flow(value: &str) -> Option<FlowState> {
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A cryptographically random, URL-safe token — used for `state`, `nonce`,
/// and the PKCE verifier alike. 32 bytes of entropy, same as
/// `sessions::generate_session_token`.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The S256 PKCE challenge for `verifier`: `base64url(SHA256(verifier))`,
/// unpadded. `plain` is never used — this crate only ever speaks S256.
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Query parameters accepted by `login`. `return_to` is caller-supplied and
/// therefore untrusted — see `sanitize_return_to`, which is the only thing
/// that may write it into the `FlowState`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct LoginParams {
    pub return_to: Option<String>,
}

/// Starts a login: mints `state`/`nonce`/a PKCE verifier, remembers them in
/// the flow cookie, and sends the browser to the provider. Cannot fail —
/// there is no user input yet and no network call, only local randomness —
/// so this returns a bare `Response`, not a `Result`.
#[utoipa::path(
    get, path = "/auth/oidc/login", tag = "auth",
    params(LoginParams),
    responses(
        (status = 302, description = "Redirects to the identity provider's authorization endpoint"),
    ),
)]
pub async fn login(
    State(state): State<AppState>,
    Extension(auth_state): Extension<AuthApiState>,
    Query(params): Query<LoginParams>,
) -> Response {
    start_flow(&state, &auth_state, sanitize_return_to(params.return_to.as_deref()), None)
}

/// The invite link: starts a login with the invite token bound to the flow,
/// and does nothing else. Whether the token is any good is decided only after
/// the provider has verified who is signing in.
#[utoipa::path(
    get, path = "/auth/invite/{token}", tag = "auth",
    params(("token" = String, Path, description = "The invite link token")),
    responses(
        (status = 302, description = "Redirects to the identity provider's authorization endpoint"),
    ),
)]
pub async fn invite(
    State(state): State<AppState>,
    Extension(auth_state): Extension<AuthApiState>,
    Path(token): Path<String>,
) -> Response {
    start_flow(&state, &auth_state, INVITE_LANDING.to_string(), Some(token))
}

/// Where an accepted invite lands: the trader app, never the admin console at
/// `/`. Invitees are traders, PMs and viewers; the cockpit is admin-only.
const INVITE_LANDING: &str = "/trade/";

fn start_flow(
    state: &AppState,
    auth_state: &AuthApiState,
    return_to: String,
    invite_token: Option<String>,
) -> Response {
    let flow = FlowState {
        state: random_token(),
        nonce: random_token(),
        pkce_verifier: random_token(),
        return_to,
        invite_token,
    };
    let challenge = pkce_challenge(&flow.pkce_verifier);
    let authorize_url = auth_state.provider.authorize_url(&flow.state, &flow.nonce, &challenge);

    let mut response = Redirect::to(&authorize_url).into_response();
    let cookie = flow_cookie(&state.session_config.cookie_policy, &flow);
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().expect("flow cookie header value is plain ASCII"),
    );
    response
}

/// What the provider sent back on `GET /auth/callback`. `code` and `state`
/// are `Option` (rather than required) because a provider error omits both —
/// forcing that case through a deserialization failure would make it
/// indistinguishable from a client sending a genuinely malformed request.
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// The decision table for a callback request, before anything is exchanged
/// or even looked up against the flow cookie. Kept as a pure function of
/// `CallbackParams` so it's testable without a provider, a pool, or a cookie.
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackOutcome {
    /// A `code` and `state` both arrived — the only shape worth proceeding
    /// with.
    Ok { code: String, state: String },
    /// The provider itself refused or aborted the login (e.g.
    /// `error=access_denied`). Surfaced, not swallowed: this must reach the
    /// caller as a distinct, logged failure rather than falling through to
    /// "malformed".
    ProviderError(String),
    /// Neither of the above — missing `code` or `state` with no `error`
    /// either. Not a shape a real provider (or a real error) produces.
    Malformed,
}

/// Classifies a callback request. `error` wins even if `code`/`state` also
/// happen to be present — a provider is never expected to send both, but if
/// one did, the error is the more truthful signal.
pub fn classify_callback(params: &CallbackParams) -> CallbackOutcome {
    if let Some(error) = &params.error {
        return CallbackOutcome::ProviderError(error.clone());
    }
    match (&params.code, &params.state) {
        (Some(code), Some(state)) => CallbackOutcome::Ok { code: code.clone(), state: state.clone() },
        _ => CallbackOutcome::Malformed,
    }
}

/// Whether the `state` the provider echoed back matches the one minted for
/// this flow. Checked before the authorization code is ever exchanged — an
/// attacker-supplied `state` (or a stale one from a different, abandoned
/// flow) must never reach the network call.
pub fn callback_state_matches(flow: &FlowState, provided_state: &str) -> bool {
    flow.state == provided_state
}

/// A login failure whose detail must not reach the client — see the
/// `OidcError` doc comment on never letting provider/transport detail leak
/// into a response body. The caller has already logged the real reason.
fn login_failed(status: StatusCode) -> ApiError {
    ApiError { status, message: "login failed".to_string() }
}

/// Maps an `OidcError` from `exchange_code` to a response status without
/// ever putting the error's own message (which can carry transport or
/// provider response detail) into that response. `ProviderUnavailable` is
/// genuinely an upstream problem (502); every other variant means the token
/// itself did not check out, which from the client's perspective is the same
/// as a bad credential (401).
fn map_oidc_error(err: OidcError) -> ApiError {
    let status = match err {
        OidcError::ProviderUnavailable(ref detail) => {
            tracing::warn!(detail, "oidc provider unavailable during code exchange");
            StatusCode::BAD_GATEWAY
        }
        other => {
            tracing::warn!(error = ?other, "oidc code exchange produced an untrusted token");
            StatusCode::UNAUTHORIZED
        }
    };
    login_failed(status)
}

fn db_error(context: &str, err: sqlx::Error) -> ApiError {
    tracing::error!(error = ?err, context, "database error in auth_api");
    ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "internal error".to_string() }
}

/// Completes a login: validates the callback, exchanges the code, resolves
/// the identity (claiming the flow's invite if it is unknown), and mints a
/// session. A login that resolves to no user is redirected to the access
/// page with the reason; nothing is created for it.
///
/// Every failure path returns before `create_session` is ever called: a
/// provider error, a missing/expired flow cookie, a `state` mismatch, an
/// exchange/token failure or the claim gate end in an `ApiError`; a
/// `ClaimOutcome` refusal ends in the access-page redirect.
#[utoipa::path(
    get, path = "/auth/oidc/callback", tag = "auth",
    params(CallbackParams),
    responses(
        (status = 302, description = "Login completed (session cookie set, redirects to the validated return_to), or refused (redirects to /trade/access?reason=...)"),
        (status = 400, description = "Malformed callback, or a missing/expired/mismatched login flow"),
        (status = 401, description = "The provider rejected the login, or the exchanged token failed verification"),
        (status = 403, description = "The claim gate rejected the identity"),
        (status = 502, description = "The identity provider was unreachable"),
    ),
)]
pub async fn callback(
    State(state): State<AppState>,
    Extension(auth_state): Extension<AuthApiState>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> Result<Response, ApiError> {
    let (code, provided_state) = match classify_callback(&params) {
        CallbackOutcome::ProviderError(error) => {
            tracing::warn!(error, "oidc provider returned an error on callback");
            return Err(login_failed(StatusCode::UNAUTHORIZED));
        }
        CallbackOutcome::Malformed => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "malformed callback".to_string(),
            });
        }
        CallbackOutcome::Ok { code, state } => (code, state),
    };

    let policy = &state.session_config.cookie_policy;
    let flow = flow_from_cookie(&headers, policy).ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "missing or expired login flow".to_string(),
    })?;

    // Checked before the code is exchanged: an attacker-supplied or replayed
    // `state` must never reach the network.
    if !callback_state_matches(&flow, &provided_state) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "state mismatch".to_string(),
        });
    }

    let identity = auth_state
        .provider
        .exchange_code(&code, &flow.pkce_verifier, &flow.nonce)
        .await
        .map_err(map_oidc_error)?;

    // Checked before any lookup: a login outside the configured slice of the
    // tenant is refused outright.
    if let Some((claim_name, required_value)) = auth_state.required_claim.as_ref() {
        if !claim_satisfies(&identity.claims, claim_name, required_value) {
            return Err(ApiError { status: StatusCode::FORBIDDEN, message: "not authorized".to_string() });
        }
    }

    let outcome = invites::resolve_login(state.pool(), &identity, flow.invite_token.as_deref())
        .await
        .map_err(|err| db_error("resolve_login", err))?;

    let principal_id = match outcome {
        ClaimOutcome::Linked { principal_id, .. } | ClaimOutcome::Known { principal_id, .. } => principal_id,
        refused => {
            let mut response = Redirect::to(&access_refused_url(&refused)).into_response();
            response.headers_mut().append(
                header::SET_COOKIE,
                clear_flow_cookie(policy).parse().expect("flow cookie header value is plain ASCII"),
            );
            return Ok(response);
        }
    };

    let user_agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    // The MFA policy is evaluated here, once: `require_mfa` only reads the stamp.
    let mfa = crate::oidc::amr_satisfied(&identity.claims, &auth_state.require_amr);
    let token = sessions::create_session(state.pool(), principal_id, sessions::AuthMethod::Oidc, mfa, &state.session_config.ttl, user_agent)
        .await
        .map_err(|err| db_error("create_session", err))?;

    // Re-validated on read-back, not trusted because we wrote it. The flow
    // cookie carries neither `__Host-` nor `Secure` on a loopback bind, so its
    // contents are attacker-influencable in exactly the place it matters least
    // to be careless: this is the redirect that mints the session.
    let mut response = Redirect::to(&sanitize_return_to(Some(&flow.return_to))).into_response();
    let response_headers = response.headers_mut();
    response_headers.append(
        header::SET_COOKIE,
        sessions::set_cookie_header(policy, &token.plaintext, state.session_config.ttl.absolute)
            .parse()
            .expect("session cookie header value is plain ASCII"),
    );
    response_headers.append(
        header::SET_COOKIE,
        clear_flow_cookie(policy).parse().expect("flow cookie header value is plain ASCII"),
    );
    Ok(response)
}

/// Local logout only: revokes our session and clears our cookie. The IdP's
/// own session is deliberately left alone — RP-initiated logout (redirecting
/// to the provider's `end_session_endpoint`) is not implemented, so signing
/// out of the OMS does not sign the user out of their other applications.
///
/// Reads the raw cookie itself rather than requiring `Extension<AuthContext>`
/// so it stays idempotent: a missing, already-revoked, or expired session
/// still gets a `clear_cookie_header` in the response instead of a 401.
#[utoipa::path(
    post, path = "/auth/logout", tag = "auth",
    responses(
        (status = 204, description = "Session revoked and cookie cleared (idempotent)"),
    ),
)]
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let policy = &state.session_config.cookie_policy;
    if let Some(value) = sessions::cookie_from_headers(&headers, sessions::cookie_name(policy)) {
        let hash = sessions::hash_session_token(&value);
        if let Some(record) = sessions::lookup_session(state.pool(), &hash)
            .await
            .map_err(|err| db_error("lookup_session", err))?
        {
            sessions::revoke_session(state.pool(), record.id)
                .await
                .map_err(|err| db_error("revoke_session", err))?;
        }
    }

    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        sessions::clear_cookie_header(policy)
            .parse()
            .expect("session cookie header value is plain ASCII"),
    );
    Ok(response)
}

/// The signed-in principal plus its granted portfolios — the shape a
/// front end needs to render itself without a second round trip.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct MeResponse {
    pub principal_id: String,
    pub code: String,
    pub display_name: Option<String>,
    pub portfolios: Vec<GrantedPortfolio>,
}

/// Who the caller is signed in as, and what they can act on.
///
/// Reuses `handlers::list_portfolios`'s exact query shape rather than a
/// widened or narrowed one, so `/auth/me`'s notion of "granted portfolios"
/// can never drift from `/portfolios`'s.
#[utoipa::path(
    get, path = "/auth/me", tag = "auth",
    responses(
        (status = 200, description = "OK", body = MeResponse),
        (status = 401, description = "Not authenticated"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn me(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<MeResponse>, ApiError> {
    let display_name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM principal WHERE id = $1")
            .bind(auth.principal_id)
            .fetch_one(state.pool())
            .await
            .map_err(|err| db_error("select principal", err))?;

    let rows = sqlx::query(
        "SELECT p.id, p.code, p.name, p.status, p.base_currency, \
                g.can_trade, g.can_view, g.can_allocate \
         FROM principal_portfolio_grant g \
         JOIN portfolio p ON p.id = g.portfolio_id \
         WHERE g.principal_id = $1 \
         ORDER BY p.code",
    )
    .bind(auth.principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| db_error("list granted portfolios", err))?;

    let portfolios = rows
        .into_iter()
        .map(|r| handlers::GrantedPortfolio {
            portfolio_id: r.get::<Uuid, _>("id").to_string(),
            code: r.get("code"),
            name: r.get("name"),
            status: r.get("status"),
            base_currency: r.get("base_currency"),
            can_trade: r.get("can_trade"),
            can_view: r.get("can_view"),
            can_allocate: r.get("can_allocate"),
        })
        .collect();

    Ok(Json(MeResponse {
        principal_id: auth.principal_id.to_string(),
        code: auth.principal_code,
        display_name,
        portfolios,
    }))
}

/// True if `claims[claim_name]` equals `required_value`, whether the claim is
/// a bare string (`"groups": "traders"`) or an array of strings
/// (`"groups": ["traders", "staff"]`). Any other shape — absent, a number, an
/// object, an array of non-strings — never satisfies the gate.
fn claim_satisfies(claims: &serde_json::Value, claim_name: &str, required_value: &str) -> bool {
    match claims.get(claim_name) {
        Some(serde_json::Value::String(s)) => s == required_value,
        Some(serde_json::Value::Array(values)) => {
            values.iter().any(|v| v.as_str() == Some(required_value))
        }
        _ => false,
    }
}

/// Where a refused login lands: the access page with the reason, plus the
/// masked invited address for an email mismatch.
pub fn access_refused_url(outcome: &ClaimOutcome) -> String {
    let reason = outcome.refusal_reason().unwrap_or("no_invite");
    match outcome {
        ClaimOutcome::EmailMismatch { invited_hint } => {
            format!("/trade/access?reason={reason}&hint={}", percent_encode(invited_hint))
        }
        _ => format!("/trade/access?reason={reason}"),
    }
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {

    #[test]
    fn an_invite_lands_in_the_trader_app() {
        assert_eq!(super::INVITE_LANDING, "/trade/");
        assert_eq!(super::sanitize_return_to(Some(super::INVITE_LANDING)), "/trade/");
    }

    use super::*;

    #[test]
    fn the_flow_cookie_survives_a_round_trip() {
        let policy = crate::sessions::cookie_policy("localhost:3001", None);
        let flow = FlowState {
            state: "st-1".into(),
            nonce: "n-1".into(),
            pkce_verifier: "v-1".into(),
            return_to: "/".into(),
            invite_token: None,
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
        let policy = crate::sessions::cookie_policy("localhost:3001", None);
        let header = flow_cookie(&policy, &FlowState {
            state: "st-1".into(), nonce: "n-1".into(), pkce_verifier: "v-1".into(), return_to: "/".into(), invite_token: None,
        });

        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Max-Age=300"));
    }

    #[test]
    fn a_callback_with_no_flow_cookie_cannot_be_trusted() {
        assert!(flow_from_cookie(&HeaderMap::new(), &crate::sessions::cookie_policy("localhost:3001", None)).is_none());
    }

    #[test]
    fn a_state_mismatch_is_rejected_before_anything_is_exchanged() {
        let flow = FlowState { state: "expected".into(), nonce: "n".into(), pkce_verifier: "v".into(), return_to: "/".into(), invite_token: None };

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

    #[test]
    fn a_flow_cookie_on_a_public_bind_is_host_prefixed_and_secure() {
        let policy = crate::sessions::cookie_policy("0.0.0.0:3001", None);
        let header = flow_cookie(&policy, &FlowState {
            state: "st-1".into(), nonce: "n-1".into(), pkce_verifier: "v-1".into(), return_to: "/".into(), invite_token: None,
        });

        assert!(header.starts_with("__Host-oms_login_flow="));
        assert!(header.contains("Secure"));
    }

    #[test]
    fn pkce_challenge_is_deterministic_and_not_the_verifier_itself() {
        let verifier = "a-verifier-value";
        let challenge = pkce_challenge(verifier);

        assert_eq!(challenge, pkce_challenge(verifier));
        assert_ne!(challenge, verifier);
    }

    #[test]
    fn pkce_challenge_matches_the_rfc_7636_appendix_b_known_answer_vector() {
        // https://www.rfc-editor.org/rfc/rfc7636#appendix-B — a fixed
        // verifier/challenge pair. Self-consistency alone (the test above)
        // would still pass if `pkce_challenge` used, say, SHA-1 instead of
        // SHA-256: same input, same output, just the wrong transform. Pinning
        // the actual published output is what catches that — instead of
        // finding out only when a real IdP starts rejecting every login.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

        assert_eq!(pkce_challenge(verifier), expected_challenge);
    }

    #[test]
    fn a_relative_path_is_kept() {
        assert_eq!(sanitize_return_to(Some("/trade/")), "/trade/");
        assert_eq!(sanitize_return_to(Some("/trade/orders?status=filled")), "/trade/orders?status=filled");
    }

    #[test]
    fn a_missing_return_to_falls_back_to_the_root() {
        assert_eq!(sanitize_return_to(None), "/");
        assert_eq!(sanitize_return_to(Some("")), "/");
    }

    #[test]
    fn an_absolute_url_is_refused() {
        assert_eq!(sanitize_return_to(Some("https://evil.example.com/")), "/");
        assert_eq!(sanitize_return_to(Some("http://evil.example.com/")), "/");
    }

    #[test]
    fn a_protocol_relative_path_is_refused() {
        // The classic open-redirect bypass: the browser reads "//host" as a
        // scheme-relative URL and leaves the site entirely.
        assert_eq!(sanitize_return_to(Some("//evil.example.com")), "/");
        assert_eq!(sanitize_return_to(Some("//evil.example.com/path")), "/");
    }

    #[test]
    fn a_backslash_variant_is_refused() {
        // Some browsers normalise a backslash to a forward slash, making this
        // another way to write "//".
        assert_eq!(sanitize_return_to(Some("/\\evil.example.com")), "/");
        assert_eq!(sanitize_return_to(Some("\\\\evil.example.com")), "/");
    }

    #[test]
    fn a_path_that_does_not_start_with_a_slash_is_refused() {
        assert_eq!(sanitize_return_to(Some("trade/")), "/");
        assert_eq!(sanitize_return_to(Some("javascript:alert(1)")), "/");
    }

    #[test]
    fn a_control_character_is_refused() {
        // A newline or tab can be used to smuggle a second header or confuse a
        // proxy; a legitimate path never contains one.
        assert_eq!(sanitize_return_to(Some("/trade/\nSet-Cookie: x=1")), "/");
        assert_eq!(sanitize_return_to(Some("/trade/\tfoo")), "/");
    }

    #[test]
    fn the_flow_cookie_carries_the_return_to() {
        let policy = crate::sessions::cookie_policy("localhost:3001", None);
        let flow = FlowState {
            state: "st-1".into(),
            nonce: "n-1".into(),
            pkce_verifier: "v-1".into(),
            return_to: "/trade/".into(),
            invite_token: None,
        };

        let header = flow_cookie(&policy, &flow);
        let mut headers = HeaderMap::new();
        headers.insert("cookie", header.split(';').next().unwrap().parse().unwrap());

        assert_eq!(flow_from_cookie(&headers, &policy).expect("flow").return_to, "/trade/");
    }

    #[test]
    fn the_flow_cookie_carries_the_invite_token() {
        let policy = crate::sessions::cookie_policy("localhost:3001", None);
        let flow = FlowState {
            state: "st-1".into(),
            nonce: "n-1".into(),
            pkce_verifier: "v-1".into(),
            return_to: "/".into(),
            invite_token: Some("tok-1".into()),
        };

        let header = flow_cookie(&policy, &flow);
        let mut headers = HeaderMap::new();
        headers.insert("cookie", header.split(';').next().unwrap().parse().unwrap());

        assert_eq!(flow_from_cookie(&headers, &policy).expect("flow").invite_token.as_deref(), Some("tok-1"));
    }

    #[test]
    fn a_refused_login_lands_on_the_access_page_with_its_reason() {
        assert_eq!(access_refused_url(&ClaimOutcome::NoInvite), "/trade/access?reason=no_invite");
        assert_eq!(access_refused_url(&ClaimOutcome::Expired), "/trade/access?reason=expired");
        assert_eq!(access_refused_url(&ClaimOutcome::Revoked), "/trade/access?reason=revoked");
        assert_eq!(
            access_refused_url(&ClaimOutcome::EmailUnverified),
            "/trade/access?reason=email_unverified"
        );
        assert_eq!(
            access_refused_url(&ClaimOutcome::EmailMismatch { invited_hint: "j…@fundco.com".into() }),
            "/trade/access?reason=email_mismatch&hint=j%E2%80%A6%40fundco.com"
        );
    }
}
