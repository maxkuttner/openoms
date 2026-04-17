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

// validate oms token pair 
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let (key_id, secret) = extract_basic_credentials(req.headers())?;
    
    // db lookup for secret of principal 
    let row = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT k.principal_id, k.secret_hash
        FROM oms_api_key k
        JOIN oms_principal p ON p.id = k.principal_id
        WHERE k.key_id = $1 AND k.revoked_at IS NULL AND p.status = 'ACTIVE'
        "#,
    )
    .bind(&key_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|_| service_unavailable())?;

    let (principal_id, secret_hash) = row.ok_or_else(unauthorized)?;

    // bcrypt is CPU-bound — run it off the async thread pool
    let valid = tokio::task::spawn_blocking(move || bcrypt::verify(&secret, &secret_hash))
        .await
        .map_err(|_| service_unavailable())?
        .map_err(|_| unauthorized())?;

    if !valid {
        return Err(unauthorized());
    }

    req.extensions_mut().insert(AuthContext { principal_id });
    Ok(next.run(req).await)
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

fn extract_basic_credentials(headers: &header::HeaderMap) -> Result<(String, String), Response> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(unauthorized)?;

    let encoded = value.strip_prefix("Basic ").ok_or_else(unauthorized)?;

    let decoded = STANDARD.decode(encoded).map_err(|_| unauthorized())?;
    let credentials = String::from_utf8(decoded).map_err(|_| unauthorized())?;

    let (key_id, secret) = credentials.split_once(':').ok_or_else(unauthorized)?;

    if key_id.is_empty() || secret.is_empty() {
        return Err(unauthorized());
    }

    Ok((key_id.to_string(), secret.to_string()))
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
