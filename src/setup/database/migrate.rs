//! The migration runner — the port of `db/scripts/migrate.sh`.
//!
//! Semantics are deliberately identical to the script's, because an existing
//! database must see no change: same tracking table, same two targets, same
//! per-file transaction. The 43 rows already in `public._mdm_migrations` are
//! honoured, so `migrate` on the current dev database applies nothing.
//!
//! `\i` becomes reading an embedded file, and `SET ROLE` is plain SQL that sqlx's
//! simple query protocol runs happily alongside the rest of the file — which is
//! what makes psql unnecessary.

use sqlx::PgPool;

use super::assets::{self, MigrationTarget, TARGETS};

/// A migration that has not been applied to its target yet.
#[derive(Debug)]
pub struct Pending {
    pub schema: &'static str,
    pub filename: String,
}

/// Create each schema and the shared tracking table.
///
/// `AUTHORIZATION <owner>` matches the script: objects a migration creates end up
/// owned by the role that owns the schema.
pub async fn ensure_tracking(pool: &PgPool) -> Result<(), sqlx::Error> {
    for t in &TARGETS {
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA IF NOT EXISTS {} AUTHORIZATION {};",
            t.schema, t.owner
        ))
        .execute(pool)
        .await?;
    }
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS public._mdm_migrations (
             target     text NOT NULL,
             filename   text NOT NULL,
             applied_at timestamptz NOT NULL DEFAULT now(),
             PRIMARY KEY (target, filename)
         );",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether the tracking table exists, without creating it. `status` uses this —
/// unlike `init`/`migrate`, a read-only inspection must never issue `CREATE
/// SCHEMA`/`CREATE TABLE`.
pub async fn is_migrated(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public._mdm_migrations')::text")
            .fetch_one(pool)
            .await?;
    Ok(found.is_some())
}

async fn is_applied(pool: &PgPool, schema: &str, filename: &str) -> Result<bool, sqlx::Error> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM public._mdm_migrations WHERE target = $1 AND filename = $2",
    )
    .bind(schema)
    .bind(filename)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Everything not yet applied, in apply order.
pub async fn pending(pool: &PgPool) -> Result<Vec<Pending>, sqlx::Error> {
    let mut out = Vec::new();
    for t in &TARGETS {
        for (filename, _) in assets::migrations(t) {
            if !is_applied(pool, t.schema, &filename).await? {
                out.push(Pending { schema: t.schema, filename });
            }
        }
    }
    Ok(out)
}

/// How many migrations this database has recorded.
pub async fn applied_count(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM public._mdm_migrations")
        .fetch_one(pool)
        .await
}

/// Apply every pending migration. Returns how many ran.
pub async fn apply_all(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut applied = 0;
    for t in &TARGETS {
        for (filename, sql) in assets::migrations(t) {
            if is_applied(pool, t.schema, &filename).await? {
                continue;
            }
            apply_one(pool, t, &filename, sql).await?;
            applied += 1;
        }
    }
    Ok(applied)
}

/// One migration, one transaction: become the owner, set the search path, run the
/// file, then record it. Recording inside the same transaction is what makes a
/// failed migration leave no trace.
async fn apply_one(
    pool: &PgPool,
    target: &MigrationTarget,
    filename: &str,
    sql: &str,
) -> Result<(), sqlx::Error> {
    tracing::info!("[{}] applying {}", target.schema, filename);
    let mut tx = pool.begin().await?;

    sqlx::raw_sql(&format!(
        "SET ROLE {}; SET search_path TO {};",
        target.owner, target.schema
    ))
    .execute(&mut *tx)
    .await?;

    sqlx::raw_sql(sql).execute(&mut *tx).await?;

    // Back to the connecting role before writing the tracking row — the table is
    // owned by the admin, not by the migration's owner role.
    sqlx::raw_sql("RESET ROLE;").execute(&mut *tx).await?;
    sqlx::query("INSERT INTO public._mdm_migrations (target, filename) VALUES ($1, $2)")
        .bind(target.schema)
        .bind(filename)
        .execute(&mut *tx)
        .await?;

    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::database::config;

    /// Full round trip against a real server: applying twice must be a no-op the
    /// second time, regardless of how much was already applied before this test ran
    /// (a fresh database, one already fully migrated by `oms database init`, or
    /// anything in between). Idempotency is the whole contract of `migrate` — the
    /// exact migration count is pinned separately by `embeds_every_migration` in
    /// `assets.rs` — and idempotency cannot be tested without Postgres.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live Postgres reachable via the usual POSTGRES_* config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn applying_twice_is_a_no_op() {
        let cfg = config::resolve(config::PostgresOverrides::default());
        let pool = sqlx::PgPool::connect(&cfg.url()).await.expect("connect");

        ensure_tracking(&pool).await.expect("tracking table");
        apply_all(&pool).await.expect("first apply");
        let second = apply_all(&pool).await.expect("second apply");

        assert_eq!(second, 0, "second run applied {second} migrations; expected 0");
        assert!(pending(&pool).await.expect("pending").is_empty());
    }
}
