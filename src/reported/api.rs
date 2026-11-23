//! Token-authenticated reporting API: `POST /api/v1/strategies/:code/fills`
//! and `GET /api/v1/strategies/:code/config`. Mounted outside the session-auth
//! layer: the bearer reporting token is the only credential.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use sqlx::PgPool;

use super::ingest::{self, BatchResult, FillBatch, IngestError, MAX_BATCH_FILLS, SCHEMA_VERSION};
use super::token::{self, AuthFail};
use crate::handlers::{internal, ApiError};

pub const MAX_BATCHES_PER_SEC: u32 = 20;
const BURST: f64 = 40.0;

/// Token bucket per strategy: refills at [`MAX_BATCHES_PER_SEC`], holds [`BURST`].
struct Bucket {
    tokens: f64,
    at: Instant,
}

static BUCKETS: LazyLock<Mutex<HashMap<String, Bucket>>> = LazyLock::new(Default::default);

/// Take one batch from `code`'s bucket; false when it is empty.
pub(crate) fn allow(code: &str) -> bool {
    let now = Instant::now();
    let mut buckets = BUCKETS.lock().unwrap_or_else(|p| p.into_inner());
    let b = buckets.entry(code.to_string()).or_insert(Bucket { tokens: BURST, at: now });
    b.tokens = (b.tokens + now.duration_since(b.at).as_secs_f64() * f64::from(MAX_BATCHES_PER_SEC)).min(BURST);
    b.at = now;
    if b.tokens >= 1.0 {
        b.tokens -= 1.0;
        true
    } else {
        false
    }
}

fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

/// Bearer token → its strategy, which must be the path's `code`.
async fn authorize(pool: &PgPool, headers: &HeaderMap, code: &str) -> Result<(), ApiError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
    let strategy = token::authenticate(pool, bearer.trim()).await.map_err(|e| match e {
        AuthFail::Invalid => err(StatusCode::UNAUTHORIZED, "invalid reporting token"),
        AuthFail::Db(e) => internal("database error", e),
    })?;
    if strategy != code {
        return Err(err(StatusCode::FORBIDDEN, "token does not report for this strategy"));
    }
    Ok(())
}

async fn post_fills(
    State(pool): State<PgPool>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(batch): Json<FillBatch>,
) -> Result<Json<BatchResult>, ApiError> {
    authorize(&pool, &headers, &code).await?;
    if !allow(&code) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded"));
    }
    ingest::ingest(&pool, &code, batch).await.map(Json).map_err(|e| match e {
        IngestError::SchemaVersion(v) => {
            err(StatusCode::BAD_REQUEST, format!("unsupported schema_version {v} (expected {SCHEMA_VERSION})"))
        }
        IngestError::TooLarge(n) => {
            err(StatusCode::PAYLOAD_TOO_LARGE, format!("{n} fills exceeds the {MAX_BATCH_FILLS}-fill batch limit"))
        }
        IngestError::UnknownStrategy => err(StatusCode::NOT_FOUND, "unknown strategy"),
        IngestError::Db(e) => internal("database error", e),
    })
}

async fn get_config(
    State(pool): State<PgPool>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&pool, &headers, &code).await?;
    let portfolio_code: String = sqlx::query_scalar(
        "SELECT p.code FROM strategy s JOIN portfolio p ON p.id = s.portfolio_id WHERE s.code = $1",
    )
    .bind(&code)
    .fetch_one(&pool)
    .await
    .map_err(|e| internal("database error", e))?;
    Ok(Json(json!({
        "strategy": code,
        "portfolio_code": portfolio_code,
        "schema_version": SCHEMA_VERSION,
        "max_batch_fills": MAX_BATCH_FILLS,
        "max_batches_per_sec": MAX_BATCHES_PER_SEC,
    })))
}

/// The reporting routes, carrying their own pool state (merge into any router).
pub fn router<S: Clone + Send + Sync + 'static>(pool: PgPool) -> Router<S> {
    Router::new()
        .route("/api/v1/strategies/:code/fills", post(post_fills))
        .route("/api/v1/strategies/:code/config", get(get_config))
        .with_state(pool)
}
