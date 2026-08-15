//! Role attributes and cross-schema grants — the port of `db/scripts/access.sh`.
//!
//! Runs after migrations so `GRANT ON ALL TABLES` catches every object that
//! exists, exactly as the script required.

use sqlx::PgPool;

use super::assets;

pub async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    for (name, sql) in assets::access_sql() {
        tracing::info!("applying access policy {name}");
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    Ok(())
}
