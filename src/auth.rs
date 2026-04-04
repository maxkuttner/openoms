use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::Response,
};
use jsonwebtoken::{
    decode, decode_header, Algorithm, DecodingKey, Validation,
    jwk::JwkSet,
};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::app_state::AppState;

const JWKS_CACHE_TTL: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct AuthConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_url: String,
    jwks_cache: Arc<RwLock<JwksCache>>,
}

impl AuthConfig {
    pub fn new(issuer: String, audience: String, jwks_url: String) -> Self {
        Self {
            issuer,
            audience,
            jwks_url,
            jwks_cache: Arc::new(RwLock::new(JwksCache::empty())),
        }
    }
}

#[derive(Clone)]
pub struct AuthContext {
    pub sub: String,
}

#[derive(Default)]
struct JwksCache {
    jwks: Option<Arc<JwkSet>>,
    fetched_at: Option<Instant>,
}

impl JwksCache {
    fn empty() -> Self {
        Self {
            jwks: None,
            fetched_at: None,
        }
    }

    fn is_fresh(&self) -> bool {
        match self.fetched_at {
            Some(time) => time.elapsed() < JWKS_CACHE_TTL,
            None => false,
        }
    }
}

#[derive(Debug)]
enum AuthError {
    MissingToken,
    InvalidToken,
    JwksFetchFailed,
    JwkNotFound,
    KeyDecodeFailed,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
    iss: Option<String>,
    aud: Option<serde_json::Value>,
    nbf: Option<usize>,
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let token = extract_bearer_token(req.headers())?;
    let claims = verify_jwt(&state.auth, &token).await?;
    req.extensions_mut().insert(AuthContext { sub: claims.sub });
    Ok(next.run(req).await)
}

fn extract_bearer_token(headers: &header::HeaderMap) -> Result<String, Response> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| auth_error(AuthError::MissingToken))?;

    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(|| auth_error(AuthError::MissingToken))?;

    if token.is_empty() {
        return Err(auth_error(AuthError::MissingToken));
    }

    Ok(token.to_string())
}

async fn verify_jwt(config: &AuthConfig, token: &str) -> Result<Claims, Response> {
    let header = decode_header(token).map_err(|_| auth_error(AuthError::InvalidToken))?;
    let kid = header
        .kid
        .ok_or_else(|| auth_error(AuthError::InvalidToken))?;

    let jwk = get_jwk_for_kid(config, &kid)
        .await
        .map_err(auth_error)?;

    let decoding_key = DecodingKey::from_jwk(&jwk)
        .map_err(|_| auth_error(AuthError::KeyDecodeFailed))?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[config.audience.clone()]);
    validation.set_issuer(&[config.issuer.clone()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;

    let token = decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|_| auth_error(AuthError::InvalidToken))?;
    Ok(token.claims)
}

async fn get_jwk_for_kid(config: &AuthConfig, kid: &str) -> Result<jsonwebtoken::jwk::Jwk, AuthError> {
    if let Some(jwk) = read_cached_jwk(config, kid).await {
        return Ok(jwk);
    }

    refresh_jwks(config).await?;

    read_cached_jwk(config, kid)
        .await
        .ok_or(AuthError::JwkNotFound)
}

async fn read_cached_jwk(config: &AuthConfig, kid: &str) -> Option<jsonwebtoken::jwk::Jwk> {
    let cache = config.jwks_cache.read().await;
    if !cache.is_fresh() {
        return None;
    }
    let jwks = cache.jwks.as_ref()?;
    jwks.keys
        .iter()
        .find(|key| key.common.key_id.as_deref() == Some(kid))
        .cloned()
}

async fn refresh_jwks(config: &AuthConfig) -> Result<(), AuthError> {
    let jwks = fetch_jwks(&config.jwks_url).await?;
    let mut cache = config.jwks_cache.write().await;
    cache.jwks = Some(Arc::new(jwks));
    cache.fetched_at = Some(Instant::now());
    Ok(())
}

async fn fetch_jwks(url: &str) -> Result<JwkSet, AuthError> {
    let response = reqwest::get(url)
        .await
        .map_err(|_| AuthError::JwksFetchFailed)?;

    if !response.status().is_success() {
        return Err(AuthError::JwksFetchFailed);
    }

    response
        .json::<JwkSet>()
        .await
        .map_err(|_| AuthError::JwksFetchFailed)
}

fn auth_error(err: AuthError) -> Response {
    let status = match err {
        AuthError::MissingToken | AuthError::InvalidToken | AuthError::JwkNotFound | AuthError::KeyDecodeFailed => {
            StatusCode::UNAUTHORIZED
        }
        AuthError::JwksFetchFailed => StatusCode::SERVICE_UNAVAILABLE,
    };

    Response::builder()
        .status(status)
        .body(status.canonical_reason().unwrap_or("unauthorized").into())
        .unwrap()
}
