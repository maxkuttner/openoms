mod adapters;
mod db;
mod domain;
mod handlers;
mod models;
mod projector;
mod services;

use axum::{
    http::StatusCode, http::Uri, response::IntoResponse, routing::get, routing::post, Router,
};
use sqlx::PgPool;
use std::env;
use std::sync::Arc;
use tracing::{error, info, Level};
use tracing_subscriber;
mod kafka;

// Application State -> Not sure yet
pub struct AppState {
    pub pool: PgPool,
    pub order_service: services::order_service::OrderService,
}

async fn handler_404(uri: Uri) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        format!("No route found for path: {}", uri),
    )
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    let mode = env::args().nth(1);

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

    if mode.as_deref() == Some("migrate") {
        info!("Running SQL migrations");
        if let Err(e) = sqlx::migrate!().run(&pool).await {
            error!("Migration failed: {}", e);
            std::process::exit(1);
        }
        info!("Migrations complete");
        return;
    }

    if let Err(err) = db::verify_oms_contract(&pool).await {
        error!("OMS contract verification failed: {}", err);
        std::process::exit(1);
    }
    info!("OMS contract verification passed");

    let kafka_config = match kafka::KafkaConfig::from_env() {
        Ok(cfg) => cfg,
        Err(err) => {
            error!("Kafka config is invalid or missing: {}", err);
            std::process::exit(1);
        }
    };

    let kafka = match kafka_config.create_producer_client() {
        Ok(kafka_client) => kafka_client,
        Err(err) => {
            error!("Failed to connect to Kafka: {}", err);
            std::process::exit(1);
        }
    };
    info!("Connected to Kafka");

    let enable_position_projector = matches!(
        env::var("OMS_ENABLE_POSITION_PROJECTOR").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

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

    let account_repo = db::AccountRepo::new(pool.clone());
    let connector_repo = db::ConnectorRepo::new(pool.clone());
    let order_repo = db::OrderRepo::new(pool.clone());
    let alpaca_adapter = adapters::alpaca::AlpacaAdapter::new();
    let order_service = services::order_service::OrderService::new(
        account_repo,
        connector_repo,
        order_repo,
        alpaca_adapter,
        kafka,
    );

    let state = Arc::new(AppState {
        pool: pool.clone(),
        order_service,
    });

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/orders", post(handlers::create_order)) // Changed route
        .fallback(handler_404)
        .with_state(state);

    let bind_addr = match env::var("OMS_BIND_ADDR") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            error!("OMS_BIND_ADDR is not set");
            std::process::exit(1);
        }
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    info!("OMS listening on {}", bind_addr);
    axum::serve(listener, app).await.unwrap();
}
