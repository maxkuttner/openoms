//! `oms config rotate-key` — re-wrap every stored credential under a new master key.
//!
//! Rotation never touches the plaintext: each row is opened under the old key
//! and re-sealed under the new one with the same AAD (the connection `code`),
//! so the binding between a blob and its row survives untouched. Everything
//! happens inside one transaction — a failure partway must leave the store
//! fully readable under the *old* key, because there is no way to recover a
//! store that is half old-key and half new-key: no single key opens all of it.
//!
//! `credentials_updated_at` is deliberately left alone. Rotation does not
//! change the credential, only its wrapping, and an operator who sees
//! "updated 2 minutes ago" would reasonably read that as someone having
//! changed the secret itself.

use sqlx::{PgPool, Postgres, Transaction};

use crate::secrets::{self, MasterKey, SecretError};

/// Open under `old` and re-seal under `new`, keeping `code` as the AAD.
pub fn rewrap(old: &MasterKey, new: &MasterKey, code: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError> {
    let plain = secrets::open(old, code, sealed)?;
    Ok(secrets::seal(new, code, &plain))
}

/// Re-wrap every non-null `credentials` blob in `oms.broker_connection` and
/// `oms.feed_connection` from `old` to `new`, inside a single transaction.
///
/// Aborts — rolling back everything — the moment any row fails to open under
/// `old`. Skipping a bad row and continuing would leave a store where no
/// single key reads every credential, which is worse than not rotating at
/// all: the plan calls that out explicitly, and it is the reason this is one
/// transaction rather than a per-row commit.
///
/// Returns the number of rows re-wrapped.
pub async fn rotate(pool: &PgPool, old: &MasterKey, new: &MasterKey) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let n = rotate_table(&mut tx, "oms.broker_connection", old, new).await?
        + rotate_table(&mut tx, "oms.feed_connection", old, new).await?;

    tx.commit().await?;
    Ok(n)
}

/// Rotate every credential row in one table. `table` is a compile-time
/// constant supplied by `rotate` (never user input), so interpolating it into
/// the SQL text carries no injection risk.
async fn rotate_table(
    tx: &mut Transaction<'_, Postgres>,
    table: &'static str,
    old: &MasterKey,
    new: &MasterKey,
) -> Result<usize, sqlx::Error> {
    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as(&format!("SELECT code, credentials FROM {table} WHERE credentials IS NOT NULL"))
            .fetch_all(&mut **tx)
            .await?;

    let n = rows.len();
    for (code, sealed) in rows {
        // A row that will not open under `old` aborts the whole rotation — see
        // the module doc. Mapped into `sqlx::Error` only because that is this
        // function's error type; the underlying cause is a decrypt failure, not
        // a database one, and the message says so.
        let rewrapped = rewrap(old, new, &code, &sealed)
            .map_err(|e| sqlx::Error::Protocol(format!("rotate-key: {table} row '{code}': {e}")))?;

        // credentials_updated_at is intentionally untouched — see module doc.
        sqlx::query(&format!("UPDATE {table} SET credentials = $2 WHERE code = $1"))
            .bind(&code)
            .bind(&rewrapped)
            .execute(&mut **tx)
            .await?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::parse_master_key;

    /// Rotation re-wraps under the new key without touching the credential itself,
    /// and the AAD stays the connection code so the binding survives.
    #[test]
    fn rewrap_preserves_the_plaintext_and_the_binding() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        let sealed_old = crate::secrets::seal(&old, "alpaca-paper", b"payload");

        let sealed_new = rewrap(&old, &new, "alpaca-paper", &sealed_old).expect("rewrap");

        assert_eq!(crate::secrets::open(&new, "alpaca-paper", &sealed_new).expect("open"), b"payload");
        assert!(crate::secrets::open(&old, "alpaca-paper", &sealed_new).is_err(), "old key must stop working");
        assert!(crate::secrets::open(&new, "alpaca-live", &sealed_new).is_err(), "binding must survive");
    }

    /// A blob that will not open under the old key must abort the rotation rather
    /// than be dropped — losing one credential silently is worse than failing.
    #[test]
    fn rewrap_refuses_a_blob_it_cannot_open() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        assert!(rewrap(&old, &new, "alpaca-paper", &[0u8; 40]).is_err());
    }
}
