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

/// A broker connection and an account on it in `portfolio`, so a test needs
/// nothing the database does not start with. The connection's broker is made
/// up: a broker has one connection per environment. `drop_strategy` removes
/// both. Returns the account.
pub async fn seed_account(pool: &PgPool, portfolio: Uuid) -> Uuid {
    let suffix = &Uuid::new_v4().simple().to_string()[..12];
    let connection = format!("strategy-test-{suffix}");
    sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $1, 'PAPER', 'ACTIVE')")
        .bind(&connection)
        .execute(pool)
        .await
        .expect("seed broker_connection");
    let account = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $2, $2, 'ACTIVE', $3)",
    )
    .bind(account)
    .bind(&connection)
    .bind(portfolio)
    .execute(pool)
    .await
    .expect("seed account");
    account
}

/// A tradeable pair mapped on each of `brokers`, under a symbol no other test
/// uses. Returns the symbol and the instrument; `drop_pair` removes it.
pub async fn seed_pair(pool: &PgPool, brokers: &[&str]) -> (String, i64) {
    let symbol = format!("T{}USDT", &Uuid::new_v4().simple().to_string()[..8].to_uppercase());
    let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1").fetch_one(pool).await.expect("a seeded venue");
    let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1").fetch_one(pool).await.expect("a seeded currency");
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, price_precision, price_increment) \
         VALUES ($1, $2, $1, 'CRYPTO', 'SPOT', $3, 'ACTIVE', 2, 0.01) RETURNING id",
    )
    .bind(&symbol)
    .bind(&venue)
    .bind(&currency)
    .fetch_one(pool)
    .await
    .expect("seed instrument");
    for broker in brokers {
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, $3, true)")
            .bind(id)
            .bind(broker)
            .bind(&symbol)
            .execute(pool)
            .await
            .expect("seed broker_instrument");
    }
    (symbol, id)
}

pub async fn drop_pair(pool: &PgPool, instrument_id: i64) {
    sqlx::query("DELETE FROM broker_instrument WHERE instrument_id = $1").bind(instrument_id).execute(pool).await.expect("drop mappings");
    sqlx::query("DELETE FROM instrument WHERE id = $1").bind(instrument_id).execute(pool).await.expect("drop instrument");
}

/// A working order that never reached a broker (so a cancel is local), on an
/// account of the strategy's own portfolio.
pub async fn seed_open_order(pool: &PgPool, code: &str, origin: &str) -> Uuid {
    let (portfolio, principal): (Uuid, Uuid) = sqlx::query_as("SELECT portfolio_id, principal_id FROM strategy WHERE code = $1")
        .bind(code)
        .fetch_one(pool)
        .await
        .expect("strategy");
    let account = seed_account(pool, portfolio).await;
    let connection: String =
        sqlx::query_scalar("SELECT broker_connection_code FROM account WHERE id = $1").bind(account).fetch_one(pool).await.expect("account");
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
    // order_event is append-only and refuses deletes: the events stay.
    sqlx::query("DELETE FROM order_state WHERE portfolio_id = $1").bind(portfolio).execute(pool).await.expect("drop orders");
}

pub async fn drop_strategy(pool: &PgPool, code: &str) {
    let (portfolio, owner, service): (Uuid, Uuid, Uuid) =
        sqlx::query_as("SELECT portfolio_id, owner_principal_id, principal_id FROM strategy WHERE code = $1")
            .bind(code)
            .fetch_one(pool)
            .await
            .expect("strategy");
    sqlx::query("DELETE FROM strategy_connection WHERE strategy_code = $1").bind(code).execute(pool).await.expect("drop connections");
    sqlx::query("DELETE FROM strategy WHERE code = $1").bind(code).execute(pool).await.expect("drop strategy");
    let connections: Vec<String> = sqlx::query_scalar("DELETE FROM account WHERE portfolio_id = $1 RETURNING broker_connection_code")
        .bind(portfolio)
        .fetch_all(pool)
        .await
        .expect("drop accounts");
    sqlx::query("DELETE FROM broker_connection WHERE code = ANY($1) AND code LIKE 'strategy-test-%'")
        .bind(connections)
        .execute(pool)
        .await
        .expect("drop broker connections");
    sqlx::query("DELETE FROM principal_portfolio_grant WHERE portfolio_id = $1").bind(portfolio).execute(pool).await.expect("drop grants");
    sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(portfolio).execute(pool).await.expect("drop portfolio");
    sqlx::query("DELETE FROM principal WHERE id = ANY($1)").bind(vec![owner, service]).execute(pool).await.expect("drop principals");
}
