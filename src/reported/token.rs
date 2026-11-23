//! Reporting tokens: a reported strategy's bearer credential. The token is
//! `{id}.{secret}`: `id` finds the row, the secret is checked against its
//! SHA-256 (hex). The secret is 256 random bits, so a fast hash is enough and
//! keeps per-batch authentication cheap. Shown once, at issue.

use base64::{engine::general_purpose, Engine as _};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::handlers::{internal, ApiError};

/// Why a bearer token was refused.
#[derive(Debug)]
pub enum AuthFail {
    /// Malformed, unknown, revoked, expired, or its strategy is not ACTIVE.
    Invalid,
    Db(#[allow(dead_code)] sqlx::Error),
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Issue a new token for `code` on `conn`; returns the plaintext once.
pub(crate) async fn issue(conn: &mut PgConnection, code: &str) -> Result<String, ApiError> {
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    raw[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let secret = format!("sk_{}", general_purpose::URL_SAFE_NO_PAD.encode(raw));
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO strategy_reporting_token (id, strategy_code, token_hash) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(code)
        .bind(hash(&secret))
        .execute(conn)
        .await
        .map_err(|e| internal("database error", e))?;
    Ok(format!("{}.{}", id.simple(), secret))
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
    // Hash and compare on every path, so an unknown id costs the same.
    let presented = hash(secret);
    let (stored, code) = match row {
        Some((h, c)) => (h, Some(c)),
        None => ("0".repeat(64), None),
    };
    let equal: bool = presented.as_bytes().ct_eq(stored.as_bytes()).into();
    match code {
        Some(code) if equal => Ok(code),
        _ => Err(AuthFail::Invalid),
    }
}
