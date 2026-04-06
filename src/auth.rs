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
    // cache is arc read/write lock as we only want one thread writing to it
    jwks_cache: Arc<RwLock<JwksCache>>,
}

impl AuthConfig {
    // Cretae an empty auth config with 
    // issuer, audience, jwks_url and jwks cache
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
    pub org_role: Option<String>,
}


// JsonWebtokenKeySet Cache
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
            // If never fetched -> we want to heat the cache
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
    #[serde(rename = "org_role")]
    org_role: Option<String>,
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
    req.extensions_mut().insert(AuthContext {
        sub: claims.sub,
        org_role: claims.org_role,
    });
    Ok(next.run(req).await)
}

pub async fn admin_middleware(
    req: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let auth = req.extensions().get::<AuthContext>();
    let authorized = auth
        .and_then(|ctx| ctx.org_role.as_deref())
        .is_some_and(|role| role == "org:admin");

    if !authorized {
        return Err(
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body("forbidden".into())
                .unwrap(),
        );
    }

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


// Verify signed json web token
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

    // per-default we use RS256; perhaps we can outsource it to .env or config later
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[config.audience.clone()]);
    validation.set_issuer(&[config.issuer.clone()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;

    let token = decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|_| auth_error(AuthError::InvalidToken))?;
    Ok(token.claims)
}


// Use kid to get the corresponding 
// json web key for it
async fn get_jwk_for_kid(config: &AuthConfig, kid: &str) -> Result<jsonwebtoken::jwk::Jwk, AuthError> {
    // try to hit the jwk cache
    if let Some(jwk) = read_cached_jwk(config, kid).await {
        return Ok(jwk);
    }

    // if empty -> refresh the cache
    refresh_jwks(config).await?;
    
    // ... and read jwk from cache again
    read_cached_jwk(config, kid)
        .await
        .ok_or(AuthError::JwkNotFound)
}

// Read the cache from auth config 
// and see if there is a jwk for given kid
async fn read_cached_jwk(config: &AuthConfig, kid: &str) -> Option<jsonwebtoken::jwk::Jwk> {
    
    // read from the Arc<RwLock> the cache
    let cache = config.jwks_cache.read().await;
    if !cache.is_fresh() {
        return None;
    }
    // get the result of option and turn it into a reference
    let jwks = cache.jwks.as_ref()?;
    jwks.keys
        .iter()
        .find(|key| key.common.key_id.as_deref() == Some(kid))
        .cloned() // if we find a jwk set we clone it and return it
}

// Re-heat the jwks cache
async fn refresh_jwks(config: &AuthConfig) -> Result<(), AuthError> {
    let jwks = fetch_jwks(&config.jwks_url).await?;
    let mut cache = config.jwks_cache.write().await;
    cache.jwks = Some(Arc::new(jwks));
    cache.fetched_at = Some(Instant::now());
    Ok(())
}


// Fetch jwks (json web token key sets) from the JWKS Url
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
