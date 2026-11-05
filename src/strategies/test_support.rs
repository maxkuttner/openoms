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
    code
}

pub async fn drop_strategy(pool: &PgPool, code: &str) {
    let (portfolio, owner, service): (Uuid, Uuid, Uuid) =
        sqlx::query_as("SELECT portfolio_id, owner_principal_id, principal_id FROM strategy WHERE code = $1")
            .bind(code)
            .fetch_one(pool)
            .await
            .expect("strategy");
    sqlx::query("DELETE FROM strategy WHERE code = $1").bind(code).execute(pool).await.expect("drop strategy");
    sqlx::query("DELETE FROM portfolio WHERE id = $1").bind(portfolio).execute(pool).await.expect("drop portfolio");
    sqlx::query("DELETE FROM principal WHERE id = ANY($1)").bind(vec![owner, service]).execute(pool).await.expect("drop principals");
}
