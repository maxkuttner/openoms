//! DB test plumbing for the strategy tests (all `#[ignore]`, shared dev DB).

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::app_state::AppState;

/// Admin credentials from `.env`, with the `oms` role's search path (see
/// `handlers::tests::test_pool`).
pub async fn dev_pool() -> PgPool {
    use crate::setup::database::config;
    dotenvy::dotenv().ok();
    let cfg = config::resolve(config::PostgresOverrides::default());
    sqlx::postgres::PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect(&cfg.url())
        .await
        .expect("connect")
}

pub fn state(pool: PgPool) -> AppState {
    use gateway::adapter::BrokerRegistry;
    use gateway::stream_health::StreamHealthRegistry;
    use symbology::{Identifier, InMemoryCache, OpenFigiClient};
    AppState::new(
        pool,
        "test-admin-token".to_string(),
        false,
        BrokerRegistry::new(),
        Identifier::new(OpenFigiClient::new(None), InMemoryCache::new()),
        StreamHealthRegistry::new(),
        crate::event_bus::EventBus::new(),
        gateway::contract::EngineHandle::new().0,
        Arc::new(gateway::md::MarketDataGateway::new(StreamHealthRegistry::new()).0),
        crate::sessions::SessionConfig {
            cookie_policy: crate::sessions::cookie_policy("localhost:3001", None),
            ttl: crate::sessions::SessionTtl::default(),
            public_base_url: None,
        },
    )
}

async fn principal(pool: &PgPool, kind: &str, suffix: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, $3, $2, 'ACTIVE')",
    )
    .bind(id)
    .bind(format!("strategy-test-{kind}-{suffix}"))
    .bind(if kind == "owner" { "HUMAN" } else { "STRATEGY" })
    .execute(pool)
    .await
    .expect("seed principal");
    id
}

/// A strategy with its own portfolio, owner and service principal; returns its code.
pub async fn seed_strategy(pool: &PgPool) -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    let owner = principal(pool, "owner", &suffix).await;
    let service = principal(pool, "service", &suffix).await;
    let portfolio = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
        .bind(portfolio)
        .bind(format!("strategy-test-{suffix}"))
        .execute(pool)
        .await
        .expect("seed portfolio");
    let code = format!("st-{suffix}");
    sqlx::query(
        "INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, principal_id, sender_comp_id, target_comp_id) \
         VALUES ($1, $1, $2, $3, $4, $1, 'OMS')",
    )
    .bind(&code)
    .bind(portfolio)
    .bind(owner)
    .bind(service)
    .execute(pool)
    .await
    .expect("seed strategy");
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
         VALUES ($1, $2, $3, true, true, false)",
    )
    .bind(Uuid::new_v4())
    .bind(service)
    .bind(portfolio)
    .execute(pool)
    .await
    .expect("seed grant");
    code
}

/// A working order that never reached a broker (so a cancel is local), on an
/// existing account of the shared dev DB.
pub async fn seed_open_order(pool: &PgPool, code: &str, origin: &str) -> Uuid {
    let (portfolio, principal): (Uuid, Uuid) = sqlx::query_as("SELECT portfolio_id, principal_id FROM strategy WHERE code = $1")
        .bind(code)
        .fetch_one(pool)
        .await
        .expect("strategy");
    let (account, connection): (Uuid, String) =
        sqlx::query_as("SELECT id, broker_connection_code FROM account LIMIT 1").fetch_one(pool).await.expect("an account");
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, broker_connection_code, \
            instrument_id, side, order_type, time_in_force, limit_price, original_qty, leaves_qty, cum_qty, status, version, origin) \
         VALUES ($1, 'test', $2, $3, $4, $5, '1', 'buy', 'limit', 'gtc', 1, 1, 1, 0, 'submitted', 0, $6)",
    )
    .bind(order_id)
    .bind(principal)
    .bind(portfolio)
    .bind(account)
    .bind(connection)
    .bind(origin)
    .execute(pool)
    .await
    .expect("seed order");
    order_id
}

pub async fn order_status(pool: &PgPool, order_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM order_state WHERE order_id = $1").bind(order_id).fetch_one(pool).await.expect("order")
}

pub async fn drop_orders(pool: &PgPool, code: &str) {
    let portfolio: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1").bind(code).fetch_one(pool).await.expect("strategy");
    sqlx::query("DELETE FROM order_event WHERE order_id IN (SELECT order_id FROM order_state WHERE portfolio_id = $1)")
        .bind(portfolio).execute(pool).await.expect("drop events");
    sqlx::query("DELETE FROM order_state WHERE portfolio_id = $1").bind(portfolio).execute(pool).await.expect("drop orders");
}

pub async fn drop_strategy(pool: &PgPool, code: &str) {
    let (portfolio, owner, service): (Uuid, Uuid, Uuid) =
        sqlx::query_as("SELECT portfolio_id, owner_principal_id, principal_id FROM strategy WHERE code = $1")
            .bind(code)
            .fetch_one(pool)
            .await
            .expect("strategy");
    sqlx::query("DELETE FROM strategy WHERE code = $1").bind(code).execute(pool).await.expect("drop strategy");
    sqlx::query("DELETE FROM principal_portfolio_grant WHERE portfolio_id = $1").bind(portfolio).execute(pool).await.expect("drop grants");
    sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(portfolio).execute(pool).await.expect("drop portfolio");
    sqlx::query("DELETE FROM principal WHERE id = ANY($1)").bind(vec![owner, service]).execute(pool).await.expect("drop principals");
}
