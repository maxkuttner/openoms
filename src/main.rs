mod adapters;
mod db;
mod event_store;
mod domain;
mod handlers;
mod models;
mod projector;
mod app_state;
mod auth;
mod admin;

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
use crate::auth::AuthConfig;




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

        
    // If positions projector shall be used;
    let enable_position_projector = matches!(
        env::var("OMS_ENABLE_POSITION_PROJECTOR").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

    // ... start positions projector kafka client process to 
    // listen for certain events and apply
    // pojections to the db positions
    if enable_position_projector {
        let projector_pool = pool.clone();
        let projector_kafka_config = kafka_config.clone();
        tokio::spawn(async move {
            if let Err(err) =
                projector::run_position_projector(projector_pool, projector_kafka_config).await
            {
                error!("Position projector stopped: {}", err);
            }
        });
    }


    let auth_issuer = match env::var("OMS_AUTH_ISSUER") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_AUTH_ISSUER is not set");
            std::process::exit(1);
        }
    };

    let auth_audience = match env::var("OMS_AUTH_AUDIENCE") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_AUTH_AUDIENCE is not set");
            std::process::exit(1);
        }
    };

    let auth_jwks_url = match env::var("OMS_AUTH_JWKS_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_AUTH_JWKS_URL is not set");
            std::process::exit(1);
        }
    };

    let auth_config = AuthConfig::new(auth_issuer, auth_audience, auth_jwks_url);

    // AppState
    let state = AppState::new(pool, auth_config);

    // Register routes
    
    // 1) Register order routes
    let orders_router = Router::new()
        .route("/orders/submit", post(handlers::orders_submit))
        .route("/orders/cancel", post(handlers::orders_cancel))
        .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));
    
    // 2) Register admin routes
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
        .layer(middleware::from_fn(auth::admin_middleware))
        .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));

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
