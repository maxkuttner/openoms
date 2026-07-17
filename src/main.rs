mod adapters;
mod event_store;
mod domain;
mod handlers;
mod models;
mod app_state;
mod auth;
mod admin;
mod risk_engine;
mod positions;
mod recon;
mod symbology_resolver;
mod setup;

use crate::adapters::BrokerRegistry;
use crate::adapters::alpaca::AlpacaAdapter;
use crate::adapters::binance::BinanceAdapter;
use crate::adapters::ibkr::IbkrAdapter;
use crate::app_state::AppState;
use crate::domain::orders::commands::{SubmitOrder, CancelOrder};
use crate::handlers::{SubmitOrderRequest, Allocation, CreateAllocations, AllocationSplit, BlotterRow};
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderType, TimeInForce};
use crate::domain::identity::{Principal, Portfolio, Account, BrokerConnection};
use crate::admin::{
    CreatePrincipal, UpdatePrincipal,
    CreatePortfolio, UpdatePortfolio,
    CreateAccount, UpdateAccount,
    CreateBrokerConnection, UpdateBrokerConnection,
    CreateKey, ApiKeyRecord,
    CreateGrant, UpdateGrant,
};
use crate::domain::identity::Grant;

use axum::{
    response::Html,
    routing::get,
    routing::post,
    middleware,
    Router
};
use serde_json::json;
use sqlx::PgPool;
use std::env;
use dotenvy::dotenv;
use tracing::{error, info, warn, Level};
use tracing_subscriber;
use utoipa::OpenApi;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
mod kafka;
mod execution;
mod alpaca_stream;
mod binance_stream;
mod stream_health;
mod marks;
mod opra_stream;
mod stream_supervisor;
mod quote_feed;
mod mark_router;
mod binance_feed;

#[derive(OpenApi)]
#[openapi(
    info(title = "OMS API", version = "0.1.0"),
    paths(
        handlers::health,
        handlers::orders_submit,
        handlers::orders_cancel,
        handlers::get_order,
        handlers::get_portfolio_positions,
        handlers::create_allocations,
        handlers::list_allocations,
        handlers::get_orders_blotter,
        admin::create_principal,
        admin::list_principals,
        admin::get_principal,
        admin::update_principal,
        admin::register_principal_key,
        admin::list_principal_keys,
        admin::revoke_principal_key,
        admin::create_grant,
        admin::list_grants,
        admin::update_grant,
        admin::delete_grant,
        admin::create_portfolio,
        admin::list_portfolios,
        admin::get_portfolio,
        admin::update_portfolio,
        admin::create_account,
        admin::list_accounts,
        admin::get_account,
        admin::update_account,
        admin::create_broker_connection,
        admin::list_broker_connections,
        admin::get_broker_connection,
        admin::update_broker_connection,
        admin::create_risk_limit,
        admin::list_risk_limits,
        admin::get_risk_limit,
        admin::update_risk_limit,
        admin::delete_risk_limit,
        admin::list_instruments,
        admin::list_universes,
        admin::list_underlyings,
        admin::list_universe_symbols,
        admin::set_universe_symbols,
        admin::estimate_universe,
        admin::seed_universe,
        admin::run_recon,
        admin::list_recon_runs,
        admin::list_recon_breaks,
        admin::resolve_symbology,
        admin::backfill_symbology,
    ),
    components(schemas(
        SubmitOrder, SubmitOrderRequest, CancelOrder, OrderSide, OrderType, TimeInForce, OrderAggregateState,
        crate::positions::Position,
        Allocation, CreateAllocations, AllocationSplit, BlotterRow,
        Principal, Portfolio, Account, BrokerConnection,
        CreatePrincipal, UpdatePrincipal,
        CreatePortfolio, UpdatePortfolio,
        CreateAccount, UpdateAccount,
        CreateBrokerConnection, UpdateBrokerConnection,
        CreateKey, ApiKeyRecord,
        Grant, CreateGrant, UpdateGrant,
        admin::RiskLimit, admin::CreateRiskLimit, admin::UpdateRiskLimit,
        admin::InstrumentSummary,
        admin::UniverseSummary, admin::EstimateResponse, admin::SeedRequest, admin::SeedAccepted,
        admin::UnderlyingCandidate, admin::SetSymbolsRequest, admin::SetSymbolsResponse,
        admin::RunReconRequest, admin::ReconRunRow, admin::ReconBreakRow,
        crate::recon::ReconSummary, crate::recon::ReconBreak, crate::recon::BreakKind,
        admin::ResolveRequest, admin::BackfillRequest, admin::BackfillResult,
        crate::symbology_resolver::ResolveOutcome, crate::symbology_resolver::ResolvedIdentity,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "orders", description = "Order submission and cancellation"),
        (name = "admin", description = "Admin management of principals, portfolios, accounts, broker connections, and keys"),
    )
)]
struct ApiDoc;

struct SecurityAddon;
impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_default();
        components.add_security_scheme(
            "basic_auth",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Basic)),
        );
        components.add_security_scheme(
            "bearer_token",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}




/// OMS command-line entry point. With no subcommand it runs the server (the
/// default, preserving `default-run = "rustoms"`); `oms setup …` runs a
/// maintenance/seeding subcommand.
#[derive(clap::Parser)]
#[command(name = "oms", about = "OMS server + setup CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Setup / seeding subcommands.
    #[command(subcommand)]
    Setup(SetupCmd),
}

#[derive(clap::Subcommand)]
enum SetupCmd {
    /// Seed the master instrument universe from the configured providers.
    Universe(setup::universe::Args),
    /// Sync broker symbology (instrument_xref BROKER rows) from a broker catalog.
    SyncBrokers(setup::brokers::Args),
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    let cli = <Cli as clap::Parser>::parse();
    match cli.command {
        Some(Command::Setup(SetupCmd::Universe(args))) => {
            if let Err(e) = setup::universe::run(args).await {
                error!("setup universe failed: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Setup(SetupCmd::SyncBrokers(args))) => {
            if let Err(e) = setup::brokers::run(args).await {
                error!("setup sync-brokers failed: {e}");
                std::process::exit(1);
            }
        }
        None => serve().await,
    }
}

// Server entry point (default when no subcommand is given).
async fn serve() {

    // parse db config or panic
    let db_user = env::var("DB_USER").expect("DB_USER must be set");
    let db_host = env::var("DB_HOST").expect("DB_HOST must be set");
    let db_port = env::var("DB_PORT").expect("DB_PORT must bes set");
    let db_password = env::var("DB_PASSWORD").expect("DB_PASSWORD must be set");
    let db_name = env::var("DB_NAME").expect("DB_NAME must be set");
    let database_url = format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=disable",
        db_user, db_password, db_host, db_port, db_name
    );

    info!("Connecting to the database at {}", database_url);
    let pool = match PgPool::connect(&database_url).await {
        Ok(pool) => pool,
        Err(e) => {
            error!("Failed to connect to the database: {}", e);
            return;
        }
    };

    // Schema is owned and migrated by the `mdm` repo; OMS only connects.

    // Init kafka client from .env (optional — publishing is disabled if not configured)
    let kafka_client: Option<kafka::KafkaClient> = match kafka::KafkaConfig::from_env() {
        Ok(cfg) => match cfg.create_producer_client() {
            Ok(client) => { info!("Kafka producer ready (topic: {})", cfg.orders_topic); Some(client) }
            Err(err) => { warn!("Kafka producer failed, publishing disabled: {}", err); None }
        },
        Err(err) => { info!("Kafka not configured ({}), publishing disabled", err); None }
    };


    let admin_auth_enabled = env::var("OMS_ADMIN_AUTH_ENABLED")
        .map(|v| v.to_lowercase() != "false")
        .unwrap_or(true);

    let admin_token = if !admin_auth_enabled {
        String::new()
    } else {
        env::var("OMS_ADMIN_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                error!("OMS_ADMIN_TOKEN is not set");
                std::process::exit(1);
            })
    };

    // Build broker registry — adapters are registered only when credentials are present.
    // Env vars follow the pattern {BROKER}_{ENVIRONMENT}_{KEY}.
    let mut registry = BrokerRegistry::new();

    match (
        env::var("ALPACA_PAPER_API_KEY"),
        env::var("ALPACA_PAPER_API_SECRET"),
    ) {
        (Ok(key), Ok(secret)) if !key.is_empty() && !secret.is_empty() => {
            use std::sync::Arc;
            registry.register_alpaca("PAPER", Arc::new(AlpacaAdapter::new(key, secret, "PAPER")));
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
            registry.register_alpaca("LIVE", Arc::new(AlpacaAdapter::new(key, secret, "LIVE")));
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

    // Binance Spot Testnet → BINANCE/PAPER. Ed25519 key: an API key id plus the
    // path to its PKCS#8 PEM private key (required for the WS-API user-data stream,
    // and used for REST signing too). Keep the concrete Arc for the stream.
    let binance_paper: Option<std::sync::Arc<BinanceAdapter>> = match (
        env::var("BINANCE_PAPER_API_KEY"),
        env::var("BINANCE_PAPER_PRIVATE_KEY_PATH"),
    ) {
        (Ok(key), Ok(path)) if !key.is_empty() && !path.is_empty() => {
            use std::sync::Arc;
            match std::fs::read_to_string(&path) {
                Ok(pem) => match BinanceAdapter::new(key, &pem, "PAPER") {
                    Ok(adapter) => {
                        let adapter = Arc::new(adapter);
                        registry.register("BINANCE", "PAPER", adapter.clone());
                        info!("registered BINANCE/PAPER adapter");
                        Some(adapter)
                    }
                    Err(e) => {
                        error!("BINANCE/PAPER adapter not registered: {e}");
                        None
                    }
                },
                Err(e) => {
                    error!("BINANCE/PAPER adapter not registered: cannot read {path}: {e}");
                    None
                }
            }
        }
        _ => {
            info!("BINANCE_PAPER_API_KEY / BINANCE_PAPER_PRIVATE_KEY_PATH not set — BINANCE/PAPER adapter not registered");
            None
        }
    };

    // Symbology engine (OpenFIGI). Works without a key (lower rate limits); a key
    // (OPENFIGI_API_KEY) raises the limits and batch size.
    let openfigi_key = env::var("OPENFIGI_API_KEY").ok();
    if openfigi_key.is_some() {
        info!("OpenFIGI: using API key");
    } else {
        info!("OPENFIGI_API_KEY not set — using unauthenticated OpenFIGI (lower limits)");
    }
    let symbology = symbology::Identifier::new(
        symbology::OpenFigiClient::new(openfigi_key),
        symbology::InMemoryCache::new(),
    );

    // AppState
    let state = AppState::new(pool, admin_token, admin_auth_enabled, registry, kafka_client, symbology);

    // One-time backfill: if the position projection is empty, rebuild it from the
    // event log so existing fills are reflected. No-op on a fresh install.
    match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM position")
        .fetch_one(state.pool())
        .await
    {
        Ok(0) => match positions::rebuild_positions(state.pool()).await {
            Ok(n) => info!(positions = n, "rebuilt position projection from event log"),
            Err(e) => error!(error = ?e, "failed to rebuild position projection"),
        },
        Ok(_) => {}
        Err(e) => error!(error = ?e, "failed to check position projection"),
    }

    // Doorbell from the fill path to the marks feeds: a fill moved a position, so
    // re-read the held set. bounded(1) — a queued signal already means "reload",
    // so extras are redundant and try_send never blocks the fill path.
    //
    // The fill path rings once; an mpsc has exactly one consumer, so a fan-out task
    // (spawned below, once every feed has registered) relays to each feed's own
    // doorbell. Keeping the fill path on a single sender means execution.rs need
    // not know how many feeds exist.
    let (position_changed_tx, mut position_changed_rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut marks_doorbells: Vec<tokio::sync::mpsc::Sender<()>> = Vec::new();

    // Spawn Alpaca trade-update stream tasks (one per configured environment)
    if let (Ok(key), Ok(secret)) = (env::var("ALPACA_PAPER_API_KEY"), env::var("ALPACA_PAPER_API_SECRET")) {
        if !key.is_empty() && !secret.is_empty() {
            if let Some(adapter) = state.registry().get_alpaca("PAPER") {
                let health = state.stream_health().handle("ALPACA", "PAPER");
                tokio::spawn(alpaca_stream::run("PAPER", key, secret, state.pool().clone(), state.kafka().cloned(), adapter, health, Some(position_changed_tx.clone())));
            }
        }
    }
    if let (Ok(key), Ok(secret)) = (env::var("ALPACA_LIVE_API_KEY"), env::var("ALPACA_LIVE_API_SECRET")) {
        if !key.is_empty() && !secret.is_empty() {
            if let Some(adapter) = state.registry().get_alpaca("LIVE") {
                let health = state.stream_health().handle("ALPACA", "LIVE");
                tokio::spawn(alpaca_stream::run("LIVE", key, secret, state.pool().clone(), state.kafka().cloned(), adapter, health, Some(position_changed_tx.clone())));
            }
        }
    }

    // Spawn the Binance user-data stream when configured.
    if let Some(adapter) = binance_paper {
        let health = state.stream_health().handle("BINANCE", "PAPER");
        tokio::spawn(binance_stream::run("PAPER", state.pool().clone(), state.kafka().cloned(), adapter, health, Some(position_changed_tx.clone())));
    }

    // Market data: feeds emit quotes onto one channel; the router is the sole
    // writer to MarkStore. Adding a vendor means spawning another feed here —
    // nothing downstream changes.
    let (quote_tx, quote_rx) = tokio::sync::mpsc::channel::<dataprovider::Quote>(1024);
    tokio::spawn(mark_router::run(quote_rx, state.marks().clone()));

    if env::var("DATABENTO_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
        let (opra_pos_tx, opra_pos_rx) = tokio::sync::mpsc::channel::<()>(1);
        marks_doorbells.push(opra_pos_tx);
        let health = state.stream_health().handle("DATABENTO", "OPRA");
        let session = quote_feed::QuoteFeedSession::new(
            opra_stream::DatabentoOpraFeed,
            state.pool().clone(),
            quote_tx.clone(),
            opra_pos_rx,
            health.clone(),
        );
        tokio::spawn(stream_supervisor::supervise("DATABENTO/OPRA", health, session));
    }

    // Binance public market data — no credentials, so it is always on. Each feed
    // needs its own doorbell receiver (an mpsc has exactly one consumer), so the
    // sender is cloned per feed rather than shared.
    {
        let (binance_pos_tx, binance_pos_rx) = tokio::sync::mpsc::channel::<()>(1);
        marks_doorbells.push(binance_pos_tx);
        let health = state.stream_health().handle("BINANCE", "SPOT");
        let session = quote_feed::QuoteFeedSession::new(
            binance_feed::BinanceFeed,
            state.pool().clone(),
            quote_tx.clone(),
            binance_pos_rx,
            health.clone(),
        );
        tokio::spawn(stream_supervisor::supervise("BINANCE/SPOT", health, session));
    }

    // Relay the fill path's single doorbell to every feed. try_send: a full
    // per-feed channel already means "reload pending", and this must never block.
    tokio::spawn(async move {
        while position_changed_rx.recv().await.is_some() {
            for doorbell in &marks_doorbells {
                let _ = doorbell.try_send(());
            }
        }
    });

    // Register routes

    // 1) Register order routes
    let orders_router = Router::new()
        .route("/orders/submit", post(handlers::orders_submit))
        .route("/orders/cancel", post(handlers::orders_cancel))
        .route("/orders/:id", get(handlers::get_order))
        .route("/portfolios/:id/positions", get(handlers::get_portfolio_positions))
        .route(
            "/orders/:id/allocations",
            post(handlers::create_allocations).get(handlers::list_allocations),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));
    
    // 2) Register admin routes (protected by static bearer token only)
    let admin_router = Router::new()
        .route("/admin/orders", get(handlers::get_orders_blotter))
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
            post(admin::register_principal_key).get(admin::list_principal_keys),
        )
        .route(
            "/admin/principals/:id/keys/:key_id",
            axum::routing::delete(admin::revoke_principal_key),
        )
        .route(
            "/admin/portfolios",
            post(admin::create_portfolio).get(admin::list_portfolios),
        )
        .route(
            "/admin/portfolios/:id",
            axum::routing::patch(admin::update_portfolio).get(admin::get_portfolio),
        )
        .route(
            "/admin/accounts",
            post(admin::create_account).get(admin::list_accounts),
        )
        .route(
            "/admin/accounts/:id",
            axum::routing::patch(admin::update_account).get(admin::get_account),
        )
        .route("/admin/stream-health", get(admin::list_stream_health))
        .route(
            "/admin/broker-connections",
            post(admin::create_broker_connection).get(admin::list_broker_connections),
        )
        .route(
            "/admin/broker-connections/:code",
            axum::routing::patch(admin::update_broker_connection).get(admin::get_broker_connection),
        )
        .route(
            "/admin/principals/:id/grants",
            post(admin::create_grant).get(admin::list_grants),
        )
        .route(
            "/admin/principals/:id/grants/:grant_id",
            axum::routing::patch(admin::update_grant).delete(admin::delete_grant),
        )
        .route(
            "/admin/risk-limits",
            post(admin::create_risk_limit).get(admin::list_risk_limits),
        )
        .route(
            "/admin/risk-limits/:id",
            get(admin::get_risk_limit)
                .patch(admin::update_risk_limit)
                .delete(admin::delete_risk_limit),
        )
        .route("/admin/instruments", get(admin::list_instruments))
        .route("/admin/universes", get(admin::list_universes))
        .route("/admin/underlyings", get(admin::list_underlyings))
        .route(
            "/admin/universes/:code/symbols",
            get(admin::list_universe_symbols).put(admin::set_universe_symbols),
        )
        .route("/admin/universes/:code/estimate", get(admin::estimate_universe))
        .route("/admin/universes/:code/seed", post(admin::seed_universe))
        .route("/admin/recon/run", post(admin::run_recon))
        .route("/admin/recon/runs", get(admin::list_recon_runs))
        .route("/admin/recon/runs/:id/breaks", get(admin::list_recon_breaks))
        .route("/admin/symbology/resolve", post(admin::resolve_symbology))
        .route("/admin/symbology/backfill", post(admin::backfill_symbology))
        .layer(middleware::from_fn_with_state(state.clone(), auth::admin_middleware));

    let scalar_html = {
        let config = json!({ "url": "/api-docs/openapi.json" });
        scalar_api_reference::scalar_html_default(&config)
    };

    let app = Router::new()
        .route("/scalar", get(move || async move { Html(scalar_html) }))
        .route("/api-docs/openapi.json", get(|| async { axum::Json(ApiDoc::openapi()) }))
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
    let host_url = format!("http://{}", bind_addr);
    info!("OMS listening on {}", host_url);
    info!("Scalar UI: {}/scalar", host_url);
    info!("OpenAPI spec: {}/api-docs/openapi.json", host_url);
    axum::serve(listener, app).await.unwrap();
}
