//! Reporting tokens: a reported strategy's bearer credential. The token is
//! `{id}.{secret}`: `id` finds the row, the secret is checked against its
//! bcrypt hash (as trading tokens are). Shown once, at issue.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::handlers::{internal, ApiError};

/// Why a bearer token was refused.
#[derive(Debug)]
pub enum AuthFail {
    /// Malformed, unknown, revoked, expired, or its strategy is not ACTIVE.
    Invalid,
    Db(#[allow(dead_code)] sqlx::Error),
}

/// Issue a new token for `code` on `conn`; returns the plaintext once.
pub(crate) async fn issue(conn: &mut PgConnection, code: &str) -> Result<String, ApiError> {
    let material = crate::admin::generate_key_material().await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO strategy_reporting_token (id, strategy_code, token_hash) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(code)
        .bind(&material.secret_hash)
        .execute(conn)
        .await
        .map_err(|e| internal("database error", e))?;
    Ok(format!("{}.{}", id.simple(), material.plaintext_secret))
}

/// The strategy code a bearer token reports for. Valid iff not revoked, not
/// expired, and the strategy is ACTIVE.
pub async fn authenticate(pool: &PgPool, bearer: &str) -> Result<String, AuthFail> {
    let (id, secret) = bearer.split_once('.').ok_or(AuthFail::Invalid)?;
    let id = Uuid::parse_str(id).map_err(|_| AuthFail::Invalid)?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT t.token_hash, t.strategy_code \
           FROM strategy_reporting_token t JOIN strategy s ON s.code = t.strategy_code \
          WHERE t.id = $1 AND t.revoked_at IS NULL \
            AND (t.expires_at IS NULL OR t.expires_at > now()) \
            AND s.mode = 'REPORTED' AND s.state = 'ACTIVE'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(AuthFail::Db)?;
    let (hash, code) = row.ok_or(AuthFail::Invalid)?;
    let secret = secret.to_string();
    let ok = tokio::task::spawn_blocking(move || bcrypt::verify(&secret, &hash).unwrap_or(false))
        .await
        .unwrap_or(false);
    if ok { Ok(code) } else { Err(AuthFail::Invalid) }
}
