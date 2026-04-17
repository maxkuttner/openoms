mod adapters;
mod event_store;
mod domain;
mod handlers;
mod models;
mod app_state;
mod auth;
mod admin;

use crate::adapters::BrokerRegistry;
use crate::adapters::alpaca::AlpacaAdapter;
use crate::adapters::ibkr::IbkrAdapter;
use crate::app_state::AppState;

use axum::{
    routing::get, 
    routing::post,
    middleware,
    Router
};
use sqlx::PgPool;
use std::env;
use tracing::{error, info, Level};
use tracing_subscriber;
mod kafka;




// Main async entry point
#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    let mode = env::args().nth(1);


    // Init database connection
    let db_url = match env::var("DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("DATABASE_URL is not set");
            std::process::exit(1);
        }
    };
    info!("Connecting to the database at {}", db_url);
    let pool = match PgPool::connect(&db_url).await {
        Ok(pool) => pool,
        Err(e) => {
            error!("Failed to connect to the database: {}", e);
            return;
        }
    };
    info!("Connected to the database");


        // Migrate (mode) -> run for database migrations in ../migrations/
    if mode.as_deref() == Some("migrate") {
        info!("Running SQL migrations");
        if let Err(e) = sqlx::migrate!().run(&pool).await {
            error!("Migration failed: {}", e);
            std::process::exit(1);
        }
        info!("Migrations complete");
        return;
    }
    
    // Init kafka client from .env
    let kafka_config = match kafka::KafkaConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            error!("Kafka config is invalid or missing: {}", err);
            std::process::exit(1);
        }
    };

    let admin_token = match env::var("OMS_ADMIN_TOKEN") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_ADMIN_TOKEN is not set");
            std::process::exit(1);
        }
    };

    let admin_auth_enabled = env::var("OMS_ADMIN_AUTH_ENABLED")
        .map(|v| v.to_lowercase() != "false")
        .unwrap_or(true);

    // Build broker registry — adapters are registered only when credentials are present.
    // Env vars follow the pattern {BROKER}_{ENVIRONMENT}_{KEY}.
    let mut registry = BrokerRegistry::new();

    match (
        env::var("ALPACA_PAPER_API_KEY"),
        env::var("ALPACA_PAPER_API_SECRET"),
    ) {
        (Ok(key), Ok(secret)) if !key.is_empty() && !secret.is_empty() => {
            use std::sync::Arc;
            registry.register("ALPACA", "PAPER", Arc::new(AlpacaAdapter::new(key, secret, "PAPER")));
            info!("registered ALPACA/PAPER adapter");
        }
        _ => info!("ALPACA_PAPER_API_KEY / ALPACA_PAPER_API_SECRET not set — ALPACA/PAPER adapter not registered"),
    }

    match (
        env::var("ALPACA_LIVE_API_KEY"),
        env::var("ALPACA_LIVE_API_SECRET"),
    ) {
        (Ok(key), Ok(secret)) if !key.is_empty() && !secret.is_empty() => {
            use std::sync::Arc;
            registry.register("ALPACA", "LIVE", Arc::new(AlpacaAdapter::new(key, secret, "LIVE")));
            info!("registered ALPACA/LIVE adapter");
        }
        _ => info!("ALPACA_LIVE_API_KEY / ALPACA_LIVE_API_SECRET not set — ALPACA/LIVE adapter not registered"),
    }

    if let Ok(base_url) = env::var("IBKR_PAPER_BASE_URL") {
        if !base_url.is_empty() {
            use std::sync::Arc;
            registry.register("IBKR", "PAPER", Arc::new(IbkrAdapter::new(base_url, "PAPER")));
            info!("registered IBKR/PAPER adapter (stub)");
        }
    }

    if let Ok(base_url) = env::var("IBKR_LIVE_BASE_URL") {
        if !base_url.is_empty() {
            use std::sync::Arc;
            registry.register("IBKR", "LIVE", Arc::new(IbkrAdapter::new(base_url, "LIVE")));
            info!("registered IBKR/LIVE adapter (stub)");
        }
    }

    // AppState
    let state = AppState::new(pool, admin_token, admin_auth_enabled, registry);

    // Register routes
    
    // 1) Register order routes
    let orders_router = Router::new()
        .route("/orders/submit", post(handlers::orders_submit))
        .route("/orders/cancel", post(handlers::orders_cancel))
        .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));
    
    // 2) Register admin routes (protected by static bearer token only)
    let admin_router = Router::new()
        .route(
            "/admin/principals",
            post(admin::create_principal).get(admin::list_principals),
        )
        .route(
            "/admin/principals/:id",
            axum::routing::patch(admin::update_principal).get(admin::get_principal),
        )
        .route(
            "/admin/principals/:id/keys",
            post(admin::register_principal_key),
        )
        .route(
            "/admin/principals/:id/keys/:key_id",
            axum::routing::delete(admin::revoke_principal_key),
        )
        .route(
            "/admin/books",
            post(admin::create_book).get(admin::list_books),
        )
        .route(
            "/admin/books/:id",
            axum::routing::patch(admin::update_book).get(admin::get_book),
        )
        .route(
            "/admin/accounts",
            post(admin::create_account).get(admin::list_accounts),
        )
        .route(
            "/admin/accounts/:id",
            axum::routing::patch(admin::update_account).get(admin::get_account),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::admin_middleware));

    let app = Router::new()
        // add health check route
        .route("/health", get(handlers::health))
        .merge(orders_router)
        .merge(admin_router)
        // add 404 route as fallback
        .fallback(handlers::handler_404)
        .with_state(state);

        let bind_addr = match env::var("OMS_BIND_ADDR") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_BIND_ADDR is not set");
            std::process::exit(1);
        }
    };

    // Start TCP listener
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    info!("OMS listening on {}", bind_addr);
    axum::serve(listener, app).await.unwrap();
}
