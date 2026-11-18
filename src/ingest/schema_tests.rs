//! DB tests for the external-orders/execution-recon schema (migration 0066).

use sqlx::PgPool;
use uuid::Uuid;

use crate::authz_schema_tests::{fresh_db, migrated_db};
use crate::setup::database::migrate;

const RECON_MIGRATION: &str = "0066_";

struct Base {
    connection: String,
    account: Uuid,
    portfolio: Uuid,
    principal: Uuid,
}

/// A connection, account, portfolio and principal, using only pre-0066 columns.
async fn seed_base(pool: &PgPool) -> Base {
    let suffix = Uuid::new_v4().simple().to_string();
    let base = Base {
        connection: format!("conn-{suffix}"),
        account: Uuid::new_v4(),
        portfolio: Uuid::new_v4(),
        principal: Uuid::new_v4(),
    };
    sqlx::query("INSERT INTO oms.broker_connection (code, broker_code, environment) VALUES ($1, 'BINANCE', 'PAPER')")
        .bind(&base.connection).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
        .bind(base.portfolio).bind(format!("pf-{suffix}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')")
        .bind(base.principal).bind(format!("pr-{suffix}")).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO oms.account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $3, $4, 'ACTIVE', $5)",
    )
    .bind(base.account).bind(format!("acc-{suffix}")).bind(&base.connection)
    .bind(format!("ext-{suffix}")).bind(base.portfolio)
    .execute(pool).await.unwrap();
    base
}

async fn insert_order(
    pool: &PgPool,
    b: &Base,
    id: Uuid,
    external_order_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oms.order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, original_qty, leaves_qty, \
         cum_qty, status, external_order_id, version) \
         VALUES ($1, $2, $3, $4, $5, $6, 'BTCUSDT', 'BUY', 'MARKET', 'GTC', 1, 0, 1, 'FILLED', $7, 1)",
    )
    .bind(id).bind(format!("c-{id}")).bind(b.principal).bind(b.portfolio).bind(b.account)
    .bind(&b.connection).bind(external_order_id)
    .execute(pool).await.map(|_| ())
}

async fn seed_order(pool: &PgPool, b: &Base, external_order_id: &str) -> Uuid {
    let id = Uuid::new_v4();
    insert_order(pool, b, id, external_order_id).await.unwrap();
    id
}

async fn insert_fill(pool: &PgPool, b: &Base, exec_id: &str, order: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO oms.venue_fill (broker_connection_code, account_id, exec_id, order_id) VALUES ($1, $2, $3, $4)")
        .bind(&b.connection).bind(b.account).bind(exec_id).bind(order)
        .execute(pool).await.map(|_| ())
}

async fn insert_break(pool: &PgPool, b: &Base, venue_order_id: &str, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oms.recon_break (id, broker_connection_code, account_id, venue_order_id, kind, events_json, status) \
         VALUES ($1, $2, $3, $4, 'unknown_order', '[]'::jsonb, $5)",
    )
    .bind(Uuid::new_v4()).bind(&b.connection).bind(b.account).bind(venue_order_id).bind(status)
    .execute(pool).await.map(|_| ())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn migration_seeds_venue_fill_from_existing_fills() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < RECON_MIGRATION)
        .await
        .unwrap();

    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "venue-1").await;
    for (version, event_type, exec) in [(1, "order_partially_filled", "exec-1"), (2, "order_filled", "exec-2")] {
        sqlx::query(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, $2, $3, $4, jsonb_build_object('event_type', $4::text, \
                 'payload', jsonb_build_object('execution_id', $5::text)))",
        )
        .bind(order).bind(version).bind(Uuid::new_v4()).bind(event_type).bind(exec)
        .execute(&pool).await.unwrap();
    }
    // A non-fill event must not seed anything.
    sqlx::query(
        "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
         VALUES ($1, 3, $2, 'order_routed', '{\"payload\":{\"external_order_id\":\"venue-1\"}}')",
    )
    .bind(order).bind(Uuid::new_v4()).execute(&pool).await.unwrap();

    migrate::apply_all(&pool).await.unwrap();

    let seeded: Vec<(String, Uuid, Uuid)> = sqlx::query_as(
        "SELECT exec_id, order_id, account_id FROM oms.venue_fill WHERE broker_connection_code = $1 ORDER BY exec_id",
    )
    .bind(&b.connection).fetch_all(&pool).await.unwrap();
    assert_eq!(
        seeded,
        vec![("exec-1".to_string(), order, b.account), ("exec-2".to_string(), order, b.account)]
    );
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn migration_drops_per_trade_external_orders_sharing_a_venue_order() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < RECON_MIGRATION)
        .await
        .unwrap();

    let b = seed_base(&pool).await;
    let trade_a = seed_order(&pool, &b, "shared").await;
    let trade_b = seed_order(&pool, &b, "shared").await;
    let kept = seed_order(&pool, &b, "alone").await;
    sqlx::query("UPDATE oms.order_state SET origin = 'EXTERNAL' WHERE order_id = ANY($1)")
        .bind(vec![trade_a, trade_b, kept])
        .execute(&pool).await.unwrap();
    for order in [trade_a, trade_b, kept] {
        sqlx::query(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, 1, $2, 'order_filled', '{}')",
        )
        .bind(order).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    }

    migrate::apply_all(&pool).await.unwrap();

    let orders: Vec<Uuid> = sqlx::query_scalar("SELECT order_id FROM oms.order_state WHERE broker_connection_code = $1")
        .bind(&b.connection).fetch_all(&pool).await.unwrap();
    assert_eq!(orders, vec![kept]);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.order_event WHERE order_id = ANY($1)")
        .bind(vec![trade_a, trade_b, kept]).fetch_one(&pool).await.unwrap();
    assert_eq!(events, 1);
    // The append-only guard is back on.
    let delete = sqlx::query("DELETE FROM oms.order_event WHERE order_id = $1").bind(kept).execute(&pool).await;
    assert!(delete.is_err());
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn duplicate_venue_fill_is_rejected() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "venue-1").await;
    insert_fill(&pool, &b, "exec-1", order).await.unwrap();
    let err = insert_fill(&pool, &b, "exec-1", order).await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert_fill(&pool, &b, "exec-2", order).await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn one_open_break_per_venue_order() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    insert_break(&pool, &b, "v-1", "open").await.unwrap();
    let err = insert_break(&pool, &b, "v-1", "open").await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert_break(&pool, &b, "v-2", "open").await.unwrap();
    sqlx::query("UPDATE oms.recon_break SET status = 'resolved', resolved_at = now() WHERE venue_order_id = 'v-1'")
        .execute(&pool).await.unwrap();
    insert_break(&pool, &b, "v-1", "open").await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_venue_order_id_maps_to_one_order_per_account() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    seed_order(&pool, &b, "venue-1").await;
    let err = insert_order(&pool, &b, Uuid::new_v4(), "venue-1").await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn connection_recon_settings_default_and_validate() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    let (policy, lookback, poll): (String, String, Option<i32>) = sqlx::query_as(
        "SELECT external_policy, eod_lookback, intraday_poll_secs FROM oms.broker_connection WHERE code = $1",
    )
    .bind(&b.connection).fetch_one(&pool).await.unwrap();
    assert_eq!((policy.as_str(), lookback.as_str(), poll), ("adopt", "1w", None));
    let bad = sqlx::query("UPDATE oms.broker_connection SET external_policy = 'ignore' WHERE code = $1")
        .bind(&b.connection).execute(&pool).await;
    assert!(bad.is_err());
    let rule = sqlx::query(
        "INSERT INTO oms.attribution_rule (id, broker_connection_code, portfolio_id, priority) VALUES ($1, $2, $3, 1)",
    )
    .bind(Uuid::new_v4()).bind(&b.connection).bind(b.portfolio)
    .execute(&pool).await;
    assert!(rule.is_err(), "a rule needs an account or a client-id prefix");
}
