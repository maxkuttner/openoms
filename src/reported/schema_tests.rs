//! DB tests for the reported-strategies schema (migration 0068).

use sqlx::PgPool;
use uuid::Uuid;

use crate::authz_schema_tests::{fresh_db, migrated_db};
use crate::setup::database::migrate;

const REPORTED_MIGRATION: &str = "0068_";

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

fn is_check_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23514")
}

/// Inserts a strategy; `identity` = whether principal and comp ids are set.
async fn insert_strategy(pool: &PgPool, code: &str, mode: &str, identity: bool) -> Result<(), sqlx::Error> {
    let owner = Uuid::new_v4();
    let portfolio = Uuid::new_v4();
    sqlx::query("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
        .bind(owner).bind(format!("own-{owner}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
        .bind(portfolio).bind(format!("pf-{portfolio}")).execute(pool).await.unwrap();
    let principal = if identity {
        let p = Uuid::new_v4();
        sqlx::query("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')")
            .bind(p).bind(format!("sp-{p}")).execute(pool).await.unwrap();
        Some(p)
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO oms.strategy (code, name, portfolio_id, owner_principal_id, principal_id, \
         sender_comp_id, target_comp_id, mode) VALUES ($1, 'n', $2, $3, $4, $5, $6, $7)",
    )
    .bind(code).bind(portfolio).bind(owner).bind(principal)
    .bind(identity.then(|| format!("S-{code}"))).bind(identity.then(|| format!("T-{code}")))
    .bind(mode)
    .execute(pool).await.map(|_| ())
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn existing_strategies_still_valid_after_migration() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < REPORTED_MIGRATION)
        .await
        .unwrap();
    insert_strategy(&pool, "old", "VIA_OMS", true).await.unwrap();
    let before: (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT mode, sender_comp_id, target_comp_id FROM oms.strategy WHERE code = 'old'")
            .fetch_one(&pool).await.unwrap();

    migrate::apply_all(&pool).await.unwrap();

    let after: (String, Option<String>, Option<String>, i32) = sqlx::query_as(
        "SELECT mode, sender_comp_id, target_comp_id, stale_after_secs FROM oms.strategy WHERE code = 'old'",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!((after.0, after.1, after.2), before);
    assert_eq!(after.3, 300);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn reported_strategy_needs_no_principal() {
    let pool = migrated_db().await;
    insert_strategy(&pool, "rep", "REPORTED", false).await.unwrap();
    let err = insert_strategy(&pool, "via", "VIA_OMS", false).await.unwrap_err();
    assert!(is_check_violation(&err), "{err}");
}

async fn seed_reported(pool: &PgPool, code: &str) -> i64 {
    insert_strategy(pool, code, "REPORTED", false).await.unwrap();
    sqlx::query("INSERT INTO public.venue (code, name) VALUES ('REPVENUE', 'rv') ON CONFLICT DO NOTHING")
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO public.currency (code, name) VALUES ('REP', 'rep') ON CONFLICT DO NOTHING")
        .execute(pool).await.unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO public.instrument (symbol, name, venue, asset_class, instrument_class, currency, \
         price_precision, price_increment) VALUES ($1, 'i', 'REPVENUE', 'CRYPTO', 'SPOT', 'REP', 2, 0.01) RETURNING id",
    )
    .bind(format!("S{}", Uuid::new_v4().simple())).fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO oms.reported_batch (strategy_code, batch_id, response_json, payload_json) \
         VALUES ($1, 'b1', '{}', '{}')",
    )
    .bind(code).execute(pool).await.unwrap();
    id
}

async fn insert_reported_fill(pool: &PgPool, code: &str, instrument: i64, exec: &str, fee_effect: Option<&str>)
    -> Result<(), sqlx::Error>
{
    sqlx::query(
        "INSERT INTO oms.reported_fill (id, strategy_code, batch_id, venue, account_ref, exec_id, symbol, \
         instrument_id, side, qty, price, fee_effect, trade_time, raw_json) \
         VALUES ($1, $2, 'b1', 'BINANCE', 'acct', $3, 'BTCUSDT', $4, 'BUY', 1, 100, $5, now(), '{}')",
    )
    .bind(Uuid::new_v4()).bind(code).bind(exec).bind(instrument).bind(fee_effect)
    .execute(pool).await.map(|_| ())
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn reported_fill_dedupe_key_unique() {
    let pool = migrated_db().await;
    let inst = seed_reported(&pool, "rep").await;
    insert_reported_fill(&pool, "rep", inst, "e1", Some("Quote")).await.unwrap();
    let err = insert_reported_fill(&pool, "rep", inst, "e1", None).await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert_reported_fill(&pool, "rep", inst, "e2", Some("Base")).await.unwrap();
    insert_reported_fill(&pool, "rep", inst, "e3", Some("Other")).await.unwrap();
    let err = insert_reported_fill(&pool, "rep", inst, "e4", Some("quote")).await.unwrap_err();
    assert!(is_check_violation(&err), "{err}");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn reported_break_needs_strategy() {
    let pool = migrated_db().await;
    insert_strategy(&pool, "rep", "REPORTED", false).await.unwrap();
    let insert = |strategy: Option<&'static str>, fill: Option<&'static str>, exec: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO oms.recon_break (id, kind, source, strategy_code, reported_fill_json) \
                 VALUES ($1, 'unknown_instrument', 'REPORTED', $2, $3::jsonb)",
            )
            .bind(Uuid::new_v4()).bind(strategy)
            .bind(fill.map(|f| f.replace("EXEC", exec)))
            .execute(&pool).await.map(|_| ())
        }
    };
    let fill = r#"{"venue":"V","account_ref":"A","exec_id":"EXEC"}"#;
    let err = insert(None, Some(fill), "x").await.unwrap_err();
    assert!(is_check_violation(&err), "{err}");
    let err = insert(Some("rep"), None, "x").await.unwrap_err();
    assert!(is_check_violation(&err), "{err}");
    insert(Some("rep"), Some(fill), "x1").await.unwrap();
    let err = insert(Some("rep"), Some(fill), "x1").await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert(Some("rep"), Some(fill), "x2").await.unwrap();
    // A RECONCILED break still needs its connection, account and venue order.
    let err = sqlx::query("INSERT INTO oms.recon_break (id, kind) VALUES ($1, 'unknown_order')")
        .bind(Uuid::new_v4()).execute(&pool).await.unwrap_err();
    assert!(is_check_violation(&err), "{err}");
}
