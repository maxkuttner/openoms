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

/// Re-wrap a batch, refusing the whole batch if any row fails.
///
/// Split out of the database loop so the abort-on-first-failure rule is testable
/// without Postgres — a partially rotated store has no single key that reads all
/// of it, which is worse than not rotating, so this must never return a partial
/// result: the first row that fails to open aborts the whole call, and nothing
/// rewrapped before it is returned either.
///
/// The error carries the failing row's `code` alongside the `SecretError`: when
/// rotation aborts, "which connection" is the first thing an operator needs in
/// order to fix it, and a bare "could not decrypt" with no row identity leaves
/// them guessing at exactly the moment they are under the most pressure.
fn rewrap_rows(
    old: &MasterKey,
    new: &MasterKey,
    rows: &[(String, Vec<u8>)],
) -> Result<Vec<(String, Vec<u8>)>, (String, SecretError)> {
    rows.iter()
        .map(|(code, sealed)| {
            rewrap(old, new, code, sealed)
                .map(|out| (code.clone(), out))
                .map_err(|e| (code.clone(), e))
        })
        .collect()
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

    // A row that will not open under `old` aborts the whole rotation — see the
    // module doc and `rewrap_rows`. Mapped into `sqlx::Error` only because that
    // is this function's error type; the underlying cause is a decrypt failure,
    // not a database one. The message names the specific row so an operator
    // knows exactly which connection's credential to check, not just that
    // *something* in the table failed.
    let rewrapped = rewrap_rows(old, new, &rows).map_err(|(code, e)| {
        sqlx::Error::Protocol(format!(
            "refusing to rotate: {table} row '{code}' will not open under the current master \
             key ({e}). No rows were changed. Check that oms.master_key is the key these \
             credentials were sealed with."
        ))
    })?;

    let n = rewrapped.len();
    for (code, sealed) in rewrapped {
        // credentials_updated_at is intentionally untouched — see module doc.
        sqlx::query(&format!("UPDATE {table} SET credentials = $2 WHERE code = $1"))
            .bind(&code)
            .bind(&sealed)
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

    /// A whole batch of good rows re-wraps and every one opens under the new
    /// key — the happy path pinned at the batch level, not just per-row.
    #[test]
    fn rewrap_rows_re_wraps_every_row_when_all_open() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        let rows = vec![
            ("alpaca-paper".to_string(), secrets::seal(&old, "alpaca-paper", b"one")),
            ("databento-opra".to_string(), secrets::seal(&old, "databento-opra", b"two")),
        ];

        let out = rewrap_rows(&old, &new, &rows).expect("all rows open under old");

        assert_eq!(out.len(), 2);
        assert_eq!(secrets::open(&new, "alpaca-paper", &out[0].1).expect("open"), b"one");
        assert_eq!(secrets::open(&new, "databento-opra", &out[1].1).expect("open"), b"two");
    }

    /// One row sealed under a different key, placed second so the bad row is
    /// not simply "the first one checked" — the whole batch must still be
    /// refused, with nothing partial returned, AND the error must name the
    /// row that actually failed ('binance-paper'), not merely report that
    /// something did. An operator acting on a wrong-row message would go
    /// investigate the wrong connection.
    #[test]
    fn rewrap_rows_refuses_the_whole_batch_and_names_the_bad_row() {
        let old = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("k");
        let new = parse_master_key("base64:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").expect("k");
        let other = parse_master_key("base64:q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s=").expect("k");
        let rows = vec![
            ("alpaca-paper".to_string(), secrets::seal(&old, "alpaca-paper", b"good-one")),
            ("binance-paper".to_string(), secrets::seal(&other, "binance-paper", b"wrong-key")),
            ("databento-opra".to_string(), secrets::seal(&old, "databento-opra", b"good-two")),
        ];

        let (code, _err) =
            rewrap_rows(&old, &new, &rows).expect_err("the bad row must fail the whole batch, not just itself");

        assert_eq!(code, "binance-paper", "the error must name the row that actually failed to open");
    }
}
