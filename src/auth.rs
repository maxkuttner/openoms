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

#[derive(Clone)]
pub struct AuthContext {
    pub principal_id: Uuid,
}

/// Authenticate a trading request and inject `AuthContext { principal_id }`.
///
/// Accepts two equivalent credential forms carrying the same `(key_id, secret)`:
/// - HTTP **Basic** `key_id:secret` — the original form (kept for back-compat, incl.
///   the dev fixture and any existing callers).
/// - **Bearer** `key_id.secret` — a single copy-paste "trading token" (Databento
///   style). Split on the first `.` (neither `ak_…` key ids nor `sk_…` secrets
///   contain a dot).
///
/// Both resolve through the same DB lookup + bcrypt verify ([`verify_key`]).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let (key_id, secret) = extract_trading_credentials(req.headers())?;

    let principal_id = verify_key(state.pool(), &key_id, &secret)
        .await?
        .ok_or_else(unauthorized)?;

    req.extensions_mut().insert(AuthContext { principal_id });
    Ok(next.run(req).await)
}

/// Look up an active api key by `key_id` and bcrypt-verify `secret`. Returns the
/// owning `principal_id` on success, `None` if the key is unknown/revoked or the
/// secret doesn't match. `Err` only for infrastructure failures (DB / task join).
pub async fn verify_key(
    pool: &sqlx::PgPool,
    key_id: &str,
    secret: &str,
) -> Result<Option<Uuid>, Response> {
    let row = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT k.principal_id, k.secret_hash
        FROM api_key k
        JOIN principal p ON p.id = k.principal_id
        WHERE k.key_id = $1 AND k.revoked_at IS NULL AND p.status = 'ACTIVE'
        "#,
    )
    .bind(key_id)
    .fetch_optional(pool)
    .await
    .map_err(|_| service_unavailable())?;

    let Some((principal_id, secret_hash)) = row else {
        return Ok(None);
    };

    // bcrypt is CPU-bound — run it off the async thread pool
    let secret = secret.to_string();
    let valid = tokio::task::spawn_blocking(move || bcrypt::verify(&secret, &secret_hash))
        .await
        .map_err(|_| service_unavailable())?
        .map_err(|_| unauthorized())?;

    Ok(valid.then_some(principal_id))
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
