mod event_store;
mod order_events;
mod domain;
mod handlers;
mod models;
mod app_state;
mod auth;
mod admin;
mod instruments_api;
mod instruments_import;
mod risk_engine;
mod positions;
mod fees;
mod ingest;
mod reported;
mod reconciler;
mod recon_admin_api;
mod symbology_resolver;
mod setup;
mod sessions;
mod oidc;
mod local_auth;
mod local_auth_api;
mod auth_api;
mod accounts;
mod users_api;
mod groups_api;
mod admin_bootstrap;
mod authz;
mod limits_api;
mod grants;
mod access_grants;

use gateway::adapter::BrokerRegistry;
use gateway::md::daily_stats::DailyStatsRegistry;
use crate::app_state::AppState;
use crate::credentials::{CredentialState, FeedCredentials};
use gateway::connectors::RegistrationDeps;
use crate::reload::RegistrationOutput;
use crate::domain::orders::commands::{SubmitOrder, CancelOrder};
use crate::handlers::{SubmitOrderRequest, Allocation, CreateAllocations, AllocationSplit, BlotterRow};
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderType, TimeInForce};
use crate::domain::identity::{Principal, Portfolio, Account, BrokerConnection};
use crate::admin::{
    CreatePortfolio, UpdatePortfolio,
    CreateAccount, UpdateAccount,
    CreateBrokerConnection, UpdateBrokerConnection,
    CreateKey, ApiKeyRecord,
};

use axum::{
    response::Html,
    routing::get,
    routing::post,
    routing::delete,
    middleware,
    Extension,
    Router
};
use serde_json::json;
use sqlx::PgPool;
use std::env;
use std::sync::Arc;
use dotenvy::dotenv;
use tracing::{error, info, warn};
use tracing_subscriber::{self, EnvFilter};
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
mod kafka;
mod execution;
mod event_bus;
mod kafka_outbox;
mod live_updates;
mod order_engine;

mod venues;
mod marks;
mod daily_stats;

mod market_data;
mod order_entry;
mod colo;
mod strategies;
mod mark_router;
mod daily_stats_poller;
mod feeds;
mod preflight;
mod config;
mod secrets;
mod credentials;
mod credentials_api;
mod expiry;
mod jobs;
mod reload;
mod cockpit;
#[cfg(test)]
mod reload_tests;
#[cfg(test)]
mod boundary_tests;
#[cfg(test)]
mod authz_schema_tests;
#[cfg(test)]
mod authz_route_tests;
#[cfg(test)]
mod first_run_tests;

#[derive(OpenApi)]
#[openapi(
    info(title = "OMS API", version = "0.1.0"),
    paths(
        handlers::health,
        handlers::orders_submit,
        handlers::orders_cancel,
        handlers::get_order,
        handlers::list_orders,
        handlers::list_portfolios,
        handlers::get_portfolio_positions,
        handlers::get_portfolio_venues,
        handlers::create_allocations,
        handlers::list_allocations,
        handlers::get_orders_blotter,
        order_events::get_order_events,
        order_events::get_order_events_admin,
        instruments_api::list_instruments_for_trader,
        instruments_api::list_option_expiries,
        instruments_api::get_option_chain,
        admin::create_trading_token,
        admin::list_trading_tokens,
        admin::revoke_trading_token,
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
        admin::get_broker_connection_credentials,
        admin::put_broker_connection_credentials,
        admin::delete_broker_connection_credentials,
        jobs::api::get_connection_universe,
        jobs::api::load_connection_universe,
        jobs::api::enrich_connection_universe,
        jobs::api::list_jobs,
        jobs::api::get_job,
        admin::test_broker_connection_credentials,
        admin::list_feed_connections,
        admin::get_feed_connection_credentials,
        admin::put_feed_connection_credentials,
        admin::delete_feed_connection_credentials,
        admin::test_feed_connection_credentials,
        admin::reload_connections,
        admin::create_risk_limit,
        admin::list_risk_limits,
        admin::get_risk_limit,
        admin::update_risk_limit,
        admin::delete_risk_limit,
        admin::list_instruments,
        instruments_import::resolve_csv,
        instruments_import::save,
        instruments_import::search,
        admin::list_feeds,
        admin::resolve_symbology,
        admin::backfill_symbology,
        admin::expiry_sweep,
        users_api::setup_status,
        strategies::admin::list_strategies,
        strategies::admin::create_strategy,
        strategies::admin::update_strategy,
        strategies::admin::delete_strategy,
        strategies::admin::put_connections,
        strategies::admin::get_connections,
        strategies::admin::reset_password,
        strategies::admin::put_follow_credentials,
        strategies::admin::restart_acceptor,
        strategies::admin::strategy_securities,
        strategies::admin::kill_strategy,
        strategies::admin::enable_strategy,
        strategies::admin::handover,
        strategies::admin::rebaseline_strategy,
        strategies::admin::flatten_strategy,
        colo::deployment::register,
        colo::deployment::list,
        colo::deployment::get,
        colo::deployment::retire,
        colo::deployment::verified_stop,
        colo::ingest::adjustment,
        auth_api::login,
        auth_api::callback,
        auth_api::logout,
        auth_api::me,
        local_auth_api::signup,
        local_auth_api::login,
        local_auth_api::totp,
        local_auth_api::totp_enroll,
        local_auth_api::totp_confirm,
        local_auth_api::totp_disable,
        local_auth_api::change_password,
        local_auth_api::logout_all,
        local_auth_api::methods,
    ),
    components(schemas(
        SubmitOrder, SubmitOrderRequest, CancelOrder, OrderSide, OrderType, TimeInForce, OrderAggregateState, handlers::OrderDetail, handlers::OrderTimes,
        crate::domain::orders::state::OrderStatus,
        crate::positions::Position,
        handlers::VenueOption,
        Allocation, CreateAllocations, AllocationSplit, BlotterRow, crate::fees::FeeAmount,
        order_events::OrderEventView,
        handlers::GrantedPortfolio,
        Principal, Portfolio, Account, BrokerConnection,
        CreatePortfolio, UpdatePortfolio,
        CreateAccount, UpdateAccount,
        CreateBrokerConnection, UpdateBrokerConnection,
        admin::RedactedCredentials, crate::credentials::RedactedField,
        jobs::JobRecord, jobs::JobStatus, jobs::JobLogLine,
        instruments_import::Contract, instruments_import::ImportResolveRequest, instruments_import::ResolvedRow,
        instruments_import::SaveRequest, setup::brokers::UniverseLoadReport,
        jobs::api::UniverseStatus, jobs::api::UniverseState, jobs::api::JobStarted, jobs::api::JobDetail,
        crate::credentials_api::CredentialSubmission, admin::SaveResponse, admin::TestResponse,
        CreateKey, ApiKeyRecord,
        admin::CreateTradingToken, admin::TradingTokenCreated, admin::TradingTokenRow,
        admin::RiskLimit, admin::CreateRiskLimit, admin::UpdateRiskLimit,
        admin::InstrumentSummary, admin::FeedSummary, admin::FeedConnectionSummary,
        instruments_api::ChainLeg, instruments_api::ChainRow,
        admin::ResolveRequest, admin::BackfillRequest, admin::BackfillResult,
        admin::ExpirySweepResult,
        users_api::SetupStatus,
        crate::symbology_resolver::ResolveOutcome, crate::symbology_resolver::ResolvedIdentity,
        auth_api::MeResponse,
        local_auth_api::AuthMethods,
        local_auth_api::SignupBody,
        local_auth_api::SignupResponse,
        local_auth_api::NextStep,
        local_auth_api::LoginBody,
        local_auth_api::CodeBody,
        local_auth_api::MfaVerified,
        local_auth_api::TotpEnrollResponse,
        local_auth_api::RecoveryCodes,
        local_auth_api::ChangePasswordBody,
        strategies::admin::StrategyRow, strategies::admin::Responsible, strategies::admin::Handover, strategies::admin::CreateStrategy, strategies::admin::UpdateStrategy,
        strategies::admin::ConnectionEntry, strategies::admin::NewPassword, strategies::admin::FollowCredentials, strategies::admin::AcceptorRestarted, strategies::admin::RebaselineIssued,
        strategies::admin::TradeableInstrument, strategies::control::ControlReport,
        colo::deployment::Deployment, colo::deployment::DeploymentRegistration,
        colo::deployment::InstrumentBaseline, colo::deployment::AssetHolding,
        colo::deployment::RetireRequest, colo::ingest::AdjustmentRequest,
    )),
    modifiers(&SecurityAddon, &MfaAddon),
    tags(
        (name = "orders", description = "Order submission and cancellation"),
        (name = "admin", description = "Admin management of principals, portfolios, accounts, broker connections, and keys"),
        (name = "auth", description = "OIDC login and the signed-in principal"),
    )
)]
struct ApiDoc;

struct SecurityAddon;
impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_default();
        components.add_security_scheme(
            "basic_auth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Basic)
                    .description(Some("Username = key_id, password = secret."))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "bearer_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "User endpoints: a trading token, `key_id.secret` (Databento-style). \
                         Admin endpoints (/admin/*): the static admin token.",
                    ))
                    .build(),
            ),
        );
    }
}

/// Documents the 403 `mfa_required` on every MFA-gated route: order actions
/// (`authz::require_mfa` in the handler) and every `/admin` mutation
/// (`auth::admin_mfa_gate`). Done here rather than per handler so a new admin
/// mutation cannot forget it.
struct MfaAddon;
impl utoipa::Modify for MfaAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::{PathItemType, RefOr, ResponseBuilder};
        const TRADING: [&str; 3] = ["/orders/submit", "/orders/cancel", "/orders/{id}/allocations"];
        const NOTE: &str = "mfa_required: the session has not passed MFA (sign in again with TOTP or MFA-backed SSO)";
        for (path, item) in openapi.paths.paths.iter_mut() {
            for (method, op) in item.operations.iter_mut() {
                let gated = match method {
                    PathItemType::Get | PathItemType::Head | PathItemType::Options => false,
                    PathItemType::Post => path.starts_with("/admin/") || TRADING.contains(&path.as_str()),
                    _ => path.starts_with("/admin/"),
                };
                if !gated {
                    continue;
                }
                match op.responses.responses.get_mut("403") {
                    Some(RefOr::T(r)) => r.description = format!("{}; or {NOTE}", r.description),
                    _ => {
                        op.responses.responses.insert("403".into(), RefOr::T(ResponseBuilder::new().description(NOTE).build()));
                    }
                }
            }
        }
    }
}

/// The OpenAPI spec as pretty JSON.
///
/// Split out from the subcommand so a test can assert the spec is complete without
/// spawning a process. `docs/openapi.json` is generated from this and committed;
/// see the drift test that keeps the two in step.
fn openapi_json() -> String {
    serde_json::to_string_pretty(&ApiDoc::openapi()).expect("the OpenAPI spec must serialise")
}

/// Where the server listens when `OMS_BIND_ADDR` says nothing. Loopback by
/// design: a fresh clone should start and be reachable from a browser on the same
/// machine, and nothing more, until the operator says otherwise.
const DEFAULT_BIND_ADDR: &str = "localhost:3001";

/// Is this bind address reachable only from this machine?
///
/// Splits the host off a `host:port` pair before asking, and treats anything it
/// cannot parse as non-loopback — an unrecognised address must not be what talks
/// the server into accepting a default password.
///
/// `pub(crate)` because `sessions::cookie_policy` asks the same question of the
/// same string: one implementation, so a host can never be loopback to the
/// admin-password rule and public to the cookie rule.
pub(crate) fn bind_is_loopback(addr: &str) -> bool {
    // `[::1]:3001` — bracketed IPv6 literal, host is everything up to the bracket.
    let host = if let Some(rest) = addr.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        // `localhost:3001` / `127.0.0.1:3001`, or a bare host with no port.
        addr.rsplit_once(':').map_or(addr, |(h, _)| h)
    };
    setup::database::config::is_loopback_host(host)
}

/// Would starting with this OIDC deployment hand out a cookie the browser
/// cannot protect? If so, refuse rather than serve a broken or stealable login.
///
/// Plain http is acceptable only when *both* halves are loopback:
///
/// - The **public base URL** decides `Secure` (`sessions::cookie_policy`). An
///   `http://` URL on a public host means either no `Secure` at all, or — with
///   the policy failing closed — a `Secure` cookie the browser drops on an
///   http site. Both are broken; neither should start.
/// - The **bind address** still matters on its own: `http://localhost` as the
///   public base URL with a `0.0.0.0` bind serves a Secure-less session cookie
///   to every machine on the network, which is exactly the spec's
///   "any other bind requires both" rule.
///
/// An `https://` public base URL is always fine, whatever the bind — that is
/// the reverse-proxy topology (loopback bind, TLS terminated in front).
fn refuses_plain_http_oidc(bind_addr: &str, public_base_url: &str) -> bool {
    let https = public_base_url.starts_with("https://");
    let purely_local =
        bind_is_loopback(bind_addr) && sessions::is_plain_http_loopback(public_base_url);
    !(https || purely_local)
}

/// The client secret the bundled dev identity-provider fixture registers.
/// Anyone can read it, so it is refused anywhere but a loopback bind.
const DEV_OIDC_CLIENT_SECRET: &str = "oms-dev-secret";

fn refuses_dev_client_secret(bind_addr: &str, client_secret: Option<&str>) -> bool {
    client_secret == Some(DEV_OIDC_CLIENT_SECRET) && !bind_is_loopback(bind_addr)
}

/// Bind address on the usual tiers. No CLI flag exists for this today, so the
/// chain is env → file → default. Both sources are emptiness-filtered: a blank
/// `bind_addr` in `oms.toml` must fall through to the default the same way a
/// blank env var does, rather than reaching `TcpListener::bind` as `""`.
fn resolve_bind_addr(from_env: Option<String>, file: Option<&config::FileConfig>) -> String {
    from_env
        .filter(|v| !v.is_empty())
        .or_else(|| file.and_then(|f| f.server.bind_addr.clone()).filter(|v| !v.is_empty()))
        .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string())
}

/// Whether the credential-decrypt gate says nothing usable came back — true
/// iff something is stored and unusable AND *nothing at all* decoded. Named
/// for what it answers, not for either of its two callers: `serve()` uses it
/// to decide whether to refuse to start, and `admin::reload_connections` uses
/// the identical judgment to decide whether a reload may swap in what it just
/// built (see that function's doc comment) — a reload that would leave the
/// system in a state boot itself would have refused to start in is exactly as
/// wrong as boot starting there directly.
///
/// Pulled out as a pure function so the combinations are directly
/// table-testable — this is the most consequential new behaviour in the
/// credential-store plan (a wrong answer here either misrepresents the
/// system's state by running with no working adapters, or refuses a
/// perfectly fine partially-configured install) and it deserves more than
/// incidental coverage via a live database.
///
/// `any_configured` and `any_error` summarise every broker + feed connection's
/// decoded `CredentialState` (`Configured`/`Error`/`Unconfigured` — see
/// `credentials.rs`) after the caller has loaded them under the resolved
/// master key: `any_configured` is true iff at least one row decoded
/// successfully, `any_error` iff at least one row has a blob that did not.
/// A missing master key falls out of this for free, since with no key every
/// row that has a blob decodes straight to `Error` (see `decode`), never
/// `Configured`. A partial failure (some rows open, some do not) must still
/// pass: one bad or stale credential must not be able to disarm every other
/// one, so this is deliberately NOT `any_error` alone.
fn nothing_decrypted(any_configured: bool, any_error: bool) -> bool {
    any_error && !any_configured
}

/// OMS command-line entry point. With no subcommand it runs the server (the
/// default, preserving `default-run = "rustoms"`); `oms setup …` runs a
/// maintenance/seeding subcommand.
#[derive(clap::Parser)]
#[command(name = "oms", version, about = "OMS server + setup CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Setup / seeding subcommands.
    #[command(subcommand)]
    Setup(SetupCmd),
    /// Database provisioning and migration.
    #[command(subcommand)]
    Database(DatabaseCmd),
    /// Credential store maintenance.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Named admins: first-admin bootstrap and break-glass recovery (host only).
    #[command(subcommand)]
    Auth(AuthCmd),
    /// First-run setup: generate oms.toml and create the database.
    Init {
        /// Take values from flags and the environment instead of prompting.
        #[arg(long)]
        non_interactive: bool,
        /// Where the database lives: start PostgreSQL 16 in Docker, or use an existing one [default: existing]
        #[arg(long, value_enum)]
        postgres: Option<setup::init::rules::DbSource>,
        /// Address the server listens on [env: OMS_BIND_ADDR] [default: localhost:3001]
        #[arg(long)]
        bind: Option<String>,
        #[command(flatten)]
        db: DbArgs,
    },
    /// Show what is running on this machine and where: the OMS, its UIs, its dependencies and broker streams.
    ///
    /// Exits 1 when anything configured is down.
    Status,
    /// Print the OpenAPI spec as JSON, for the docs build and for scripting.
    Openapi,
    /// Replace this binary with the latest release (or --version), verified.
    Update {
        /// Release tag to install instead of the latest, e.g. v0.2.0. May be older (a rollback).
        #[arg(long)]
        version: Option<String>,
        /// Only report whether an update is available.
        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::Subcommand)]
enum SetupCmd {
    /// Seed the master instrument catalog + broker_instrument mapping from a broker.
    SyncBroker(setup::brokers::Args),
    /// Seed a synthetic SPY option chain — no broker, no network. For exercising
    /// instrument search / the option chain UI without a live catalog sync.
    SeedTestChain(setup::test_data::Args),
}

#[derive(clap::Subcommand)]
enum AuthCmd {
    /// Create a named admin and print a generated password (shown once).
    ///
    /// Only while no active admin exists, unless --force-recovery. On a fresh
    /// install the simpler way is to create the admin account in the cockpit.
    BootstrapAdmin {
        /// The admin's email.
        #[arg(long)]
        email: String,
        /// The admin's display name.
        #[arg(long)]
        name: Option<String>,
        /// Create the admin even though an active admin exists (break-glass).
        #[arg(long)]
        force_recovery: bool,
        #[command(flatten)]
        db: DbArgs,
    },
    /// Give an admin a new generated password (shown once), remove their
    /// TOTP and sign them out everywhere.
    RecoverAdmin {
        /// The admin's email.
        #[arg(long)]
        email: String,
        #[command(flatten)]
        db: DbArgs,
    },
}

#[derive(clap::Subcommand)]
enum ConfigCmd {
    /// Import broker and feed credentials from the environment into the store. Run once.
    ImportEnv,
    /// Re-wrap every stored credential under a freshly generated master key.
    ///
    /// Prints the new key at the end — write it into `oms.toml` (`oms.master_key`)
    /// before doing anything else. The rows are already re-wrapped when this
    /// command returns, so losing the printed key before it is saved loses every
    /// stored credential; keep the old key around until the new one is in place.
    RotateKey,
    /// Print the path of the config file in use; exit 1 when there is none.
    Path,
}

/// Connection flags shared by every database subcommand. Each falls back to its
/// `POSTGRES_*` environment variable, then to a localhost default.
#[derive(clap::Args, Clone, Default)]
struct DbArgs {
    /// Database server host [env: POSTGRES_HOST] [default: localhost]
    #[arg(long)]
    host: Option<String>,
    /// Database server port [env: POSTGRES_PORT] [default: 5432]
    #[arg(long)]
    port: Option<u16>,
    /// Superuser name [env: POSTGRES_USERNAME] [default: postgres]
    #[arg(long)]
    username: Option<String>,
    /// Superuser password [env: POSTGRES_PASSWORD] [default: postgres]
    #[arg(long)]
    password: Option<String>,
    /// Database name [env: POSTGRES_DATABASE] [default: ods]
    #[arg(long)]
    database: Option<String>,
}

// Hand-written so a stray `{:?}` — in a log line, a panic message, a clap
// debug-assert dump — cannot print the superuser password. Mirrors the
// redacting impls in `src/config.rs` and `setup::init::Prompts`.
impl std::fmt::Debug for DbArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbArgs")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("database", &self.database)
            .finish()
    }
}

impl From<DbArgs> for setup::database::config::PostgresOverrides {
    fn from(a: DbArgs) -> Self {
        Self {
            host: a.host,
            port: a.port,
            username: a.username,
            password: a.password,
            database: a.database,
        }
    }
}

#[derive(clap::Subcommand)]
enum DatabaseCmd {
    /// Create roles, database, schema, grants and reference data. Fails if any exists.
    Init {
        #[command(flatten)]
        db: DbArgs,
        /// Password for the `oms` role the server connects as [env: OMS_PASSWORD].
        /// Distinct from --password, which is the superuser's.
        #[arg(long)]
        oms_password: Option<String>,
        /// Finish an init that failed partway through: creates only whichever
        /// of the role/database is still missing, then continues straight to
        /// migrations, grants and seeding — all idempotent, so this is safe to
        /// run even if some of them already happened. Without this flag, any
        /// existing role or database is a hard refusal (unchanged default
        /// behaviour). This bypasses the wrong-server guard plain `init`
        /// provides: with `--resume` and a mistaken `--database`, migrations
        /// (several of which are `DROP …`) would be applied to an unrelated
        /// database.
        #[arg(long)]
        resume: bool,
    },
    /// Initialize the database if it does not exist, otherwise apply pending
    /// migrations. For scripts and containers.
    Setup {
        #[command(flatten)]
        db: DbArgs,
    },
    /// Apply pending migrations to an existing database.
    Migrate {
        #[command(flatten)]
        db: DbArgs,
    },
    /// Drop the database. Roles are kept.
    Drop {
        #[command(flatten)]
        db: DbArgs,
        /// Required to drop a non-loopback target. Loopback (localhost/127.0.0.1)
        /// needs no confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Show what exists and what is pending.
    Status {
        #[command(flatten)]
        db: DbArgs,
    },
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    config::load_env_beside();
    // `RUST_LOG` is honoured when set (e.g. `RUST_LOG=fix::wire=debug` to see
    // the redacted FIX wire log — see `fix::mod`'s doc comment); with no
    // `RUST_LOG` at all this falls back to plain `info`, so anyone who sets
    // nothing sees exactly what they saw before this filter existed.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // stderr, not stdout: `oms openapi` writes its spec to stdout, and a log line
    // landing there (from this prelude, or RUST_LOG picked up from a dotenv-loaded
    // .env catching a dependency's startup log) would corrupt `cargo run -- openapi
    // > docs/openapi.json`.
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    let cli = <Cli as clap::Parser>::parse();
    match cli.command {
        Some(Command::Setup(SetupCmd::SyncBroker(args))) => {
            if let Err(e) = setup::brokers::run(args).await {
                error!("setup sync-broker failed: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Setup(SetupCmd::SeedTestChain(args))) => {
            if let Err(e) = setup::test_data::run(args).await {
                error!("setup seed-test-chain failed: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Config(ConfigCmd::ImportEnv)) => {
            let key = match config::master_key(config::load()) {
                Some(Ok(key)) => key,
                Some(Err(e)) => {
                    eprintln!("error: invalid master key: {e}");
                    std::process::exit(1);
                }
                None => {
                    eprintln!(
                        "error: no master key configured — run `oms init` first, or set OMS_MASTER_KEY"
                    );
                    std::process::exit(1);
                }
            };
            let result = async {
                let pool = PgPool::connect(&setup::database_url()?).await?;
                setup::import_env::run(&pool, &key).await
            }
            .await;
            if let Err(e) = result {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Config(ConfigCmd::Path)) => {
            let path = config::path();
            if !path.exists() {
                std::process::exit(1);
            }
            println!("{}", config::absolute(path).display());
        }
        Some(Command::Config(ConfigCmd::RotateKey)) => {
            let old_key = match config::master_key(config::load()) {
                Some(Ok(key)) => key,
                Some(Err(e)) => {
                    eprintln!("error: invalid master key: {e}");
                    std::process::exit(1);
                }
                None => {
                    eprintln!(
                        "error: no master key configured — run `oms init` first, or set OMS_MASTER_KEY"
                    );
                    std::process::exit(1);
                }
            };
            let new_key_str = setup::init::generate_master_key();
            let new_key = secrets::parse_master_key(&new_key_str)
                .expect("generate_master_key always produces a value parse_master_key accepts");

            let result = async {
                let pool = PgPool::connect(&setup::database_url()?).await?;
                setup::rotate::rotate(&pool, &old_key, &new_key).await.map_err(Box::<dyn std::error::Error>::from)
            }
            .await;

            match result {
                Ok(n) => {
                    // The new key is deliberately printed — same trade `oms init` makes for
                    // the key it generates: this is the one time it can be handed to the
                    // operator at all. No credential is ever printed, only this key.
                    println!("rotated {n} sealed secret(s) (connection credentials and TOTP secrets) to a new master key.\n");
                    println!("new master key: {new_key_str}");
                    println!(
                        "\nPut this in {} as oms.master_key now — the credentials above are\n\
                         already re-wrapped under it, so this is the only remaining copy outside\n\
                         the database. Keep the OLD key somewhere safe until that edit is saved:\n\
                         if this terminal is lost before then, the old key is the only thing\n\
                         that still decrypts the store.",
                        config::path_abs().display()
                    );
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Database(cmd)) => {
            let result = match cmd {
                DatabaseCmd::Init { db, oms_password, resume } => {
                    setup::database::init(db.into(), oms_password, resume).await
                }
                DatabaseCmd::Setup { db } => setup::database::setup(db.into()).await,
                DatabaseCmd::Migrate { db } => setup::database::migrate(db.into()).await,
                DatabaseCmd::Drop { db, yes } => setup::database::drop(db.into(), yes).await,
                DatabaseCmd::Status { db } => setup::database::status(db.into()).await,
            };
            if let Err(e) = result {
                // Some of these errors already read as a user-facing message with
                // their own "error: " prefix (see already_initialized) — printing
                // that bare avoids "error: error: ...". Others (a bare sqlx error
                // from a failed connection or query, e.g. mid-`--resume`) have no
                // prefix of their own, so every other failure path in this binary
                // adds one; add it here too rather than let this one path alone
                // print unprefixed.
                let msg = e.to_string();
                if msg.starts_with("error: ") {
                    eprintln!("{msg}");
                } else {
                    eprintln!("error: {msg}");
                }
                std::process::exit(1);
            }
        }
        Some(Command::Init { non_interactive, postgres, bind, db }) => {
            // Checked here too, not only inside `run()`: `resolve()` below calls
            // `config::load()`, which exits the process over a *malformed*
            // oms.toml before `run()`'s own check ever runs — and interactively,
            // without this, the operator would type their superuser password at
            // a no-echo prompt only to be refused a moment later. `run()` keeps
            // its own check regardless; that one is the actual guarantee, this
            // one just fails earlier and more kindly.
            let cfg_path = config::absolute(config::path());
            if cfg_path.exists() {
                eprintln!("{}", setup::init::already_initialized_message(&cfg_path));
                std::process::exit(1);
            }

            let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
            // Checked before any prompt, in both modes: a bad value should not
            // cost the operator a round of typing first. Each error names where
            // the value came from — the environment may hold one (often from a
            // `.env`) the operator forgot about.
            let from = |flag: Option<String>, name: &str, var: &str| -> (Option<String>, String) {
                match flag {
                    Some(v) => (Some(v), format!("--{name}")),
                    None => (env(var), var.to_string()),
                }
            };
            let (bind, bind_src) = from(bind, "bind", "OMS_BIND_ADDR");
            let checked = bind.as_deref().map_or(Ok(()), |b| setup::init::rules::parse_bind(b).map(|_| ()))
                .map_err(|e| format!("{bind_src}: {e}"));
            if let Err(e) = checked {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            let flags = setup::init::InitFlags { source: postgres, bind };
            let container = setup::init::rules::container_mode(env("OMS_CONTAINER").as_deref());
            let answers = if non_interactive {
                let cfg = setup::database::config::resolve(db.into());
                let prompts = setup::init::Prompts {
                    host: cfg.host,
                    port: cfg.port,
                    database: cfg.database,
                    username: cfg.username,
                    password: cfg.password,
                };
                match setup::init::answers_from_flags(&flags, prompts) {
                    Ok(a) => {
                        // In the container the port is published on the host's
                        // loopback by compose; the warning would be wrong there.
                        if let Some(w) = setup::init::rules::exposure_warning(&a.bind).filter(|_| !container) {
                            eprintln!("{w}");
                        }
                        a
                    }
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            } else {
                // dialoguer needs a terminal; without one it fails on the first
                // prompt with an error that names neither the cause nor the way out.
                use std::io::IsTerminal;
                if !setup::init::prompts::can_prompt(
                    std::io::stdin().is_terminal(),
                    std::io::stderr().is_terminal(),
                ) {
                    eprintln!(
                        "error: oms init asks its questions on a terminal, and none is attached.\n\
                         \x20        Take every value from flags and the environment instead:\n\
                         \x20          oms init --non-interactive [--postgres docker] [--password '<superuser password>']"
                    );
                    std::process::exit(1);
                }
                // Seed the interactive prompts' defaults from whatever was passed
                // on the command line (and the environment, via the same
                // resolve() precedence non-interactive mode uses) — otherwise
                // `oms init --host db.internal` still prompts for host, which is
                // what made those flags pointless.
                //
                // The password is the one exception: ONLY an explicit `--password`
                // skips the prompt. `POSTGRES_PASSWORD` deliberately does not,
                // because it describes the database the *application* connects to,
                // not the one being provisioned. Honouring it here meant a repo
                // with a .env silently authenticated against a different server
                // with the wrong credential and never asked — the operator saw a
                // bare auth failure for a password they were never given a chance
                // to type.
                //
                // In the published container the rule flips: there the variable
                // is the password compose gave its own Postgres.
                let password_override = setup::init::superuser_password_default(
                    db.password.clone(),
                    env("POSTGRES_PASSWORD"),
                    container,
                );
                let cfg = setup::database::config::resolve(db.into());
                let defaults = setup::init::prompts::Defaults {
                    host: cfg.host,
                    port: cfg.port,
                    database: cfg.database,
                    username: cfg.username,
                    superuser_password: password_override,
                    bind: flags.bind.unwrap_or_else(|| setup::init::DEFAULT_BIND.into()),
                    source: flags.source,
                    container,
                };
                use setup::init::docker::DockerOps;
                // Probed only when the menu will be shown: `docker info` can take
                // seconds against a stopped Docker Desktop.
                let status = if defaults.source.is_some() || container {
                    setup::init::rules::DockerStatus::Usable
                } else {
                    setup::init::docker::Cli.status()
                };
                match setup::init::prompts::collect(&mut setup::init::prompts::Terminal::new(), &defaults, status) {
                    Ok(a) => a,
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            };
            if let Err(e) = setup::init::run(answers, &setup::init::docker::Cli).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Some(Command::Auth(cmd)) => {
            let result = match cmd {
                AuthCmd::BootstrapAdmin { email, name, force_recovery, db } => {
                    admin_bootstrap::cli_bootstrap(db.into(), &email, name.as_deref(), force_recovery).await
                }
                AuthCmd::RecoverAdmin { email, db } => admin_bootstrap::cli_recover(db.into(), &email).await,
            };
            match result {
                Ok(password) => {
                    println!(
                        "Password (shown once; sign in and change it under Account):\n\n    {password}\n"
                    );
                    if let Some(note) = admin_bootstrap::cli_password_note_for_config() {
                        println!("{note}\n");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Status) => std::process::exit(setup::status::run().await),
        Some(Command::Openapi) => println!("{}", openapi_json()),
        Some(Command::Update { version, check }) => {
            if let Err(e) = setup::update::run(setup::update::Args { version, check }).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        None => serve().await,
    }
}

/// Assembles the full application router exactly as `serve` mounts it.
/// Extracted so a test can build it: axum panics at build time on an
/// overlapping (method, path) registration, which only boots would catch.
fn build_app(
    state: AppState,
    auth_api_state: Option<auth_api::AuthApiState>,
    local_enabled: bool,
    oidc_configured: bool,
) -> Router {
    // Register routes

    // 1) Register order routes
    let orders_router = Router::new()
        .route("/orders/submit", post(handlers::orders_submit))
        .route("/orders/cancel", post(handlers::orders_cancel))
        .route("/orders/:id", get(handlers::get_order))
        .route("/orders/:id/events", get(order_events::get_order_events))
        .route("/orders", get(handlers::list_orders))
        .route("/strategies/securities", get(strategies::admin::strategy_securities))
        .route("/strategies", get(strategies::trader_api::list).post(strategies::trader_api::create))
        .route("/strategies/:code/handover", post(strategies::trader_api::handover))
        .route("/strategies/reported", get(reported::strategy::list_mine).post(reported::strategy::create))
        .route("/strategies/reported/:code", axum::routing::patch(reported::status::patch))
        .route("/strategies/reported/:code/archive", post(reported::strategy::archive))
        .route("/strategies/reported/:code/status", get(reported::status::get_status))
        .route("/strategies/reported/:code/fills", get(reported::status::get_fills))
        .route("/strategies/reported/:code/batches", get(reported::status::get_batches))
        .route(
            "/strategies/reported/:code/token",
            post(reported::strategy::rotate_token).delete(reported::strategy::revoke_token),
        )
        .route("/portfolios", get(handlers::list_portfolios))
        .route(
            "/watchlist",
            get(handlers::list_watchlist).post(handlers::add_watchlist_item),
        )
        .route("/watchlist/:instrument_id", delete(handlers::remove_watchlist_item))
        .route("/marks", get(handlers::get_marks))
        .route("/updates", get(live_updates::ws_updates))
        .route("/portfolios/:id/positions", get(handlers::get_portfolio_positions))
        .route("/portfolios/:id/venues", get(handlers::get_portfolio_venues))
        .route("/portfolios/:id/limits", get(limits_api::list_limits))
        .route(
            "/portfolios/:id/limits/:instrument_id",
            axum::routing::put(limits_api::put_limit),
        )
        .route(
            "/orders/:id/allocations",
            post(handlers::create_allocations).get(handlers::list_allocations),
        )
        .route("/instruments", get(instruments_api::list_instruments_for_trader))
        .route("/instruments/options/expiries", get(instruments_api::list_option_expiries))
        .route("/instruments/options/chain", get(instruments_api::get_option_chain))
        .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));
    
    // 2) Register admin routes: a named admin's session, or the legacy shared
    // password for reads and bootstrap only (see `auth::admin_middleware`).
    let admin_router = Router::new()
        .route("/admin/orders", get(handlers::get_orders_blotter))
        .route(
            "/admin/orders/:id/events",
            get(order_events::get_order_events_admin),
        )
        .route("/admin/users", get(users_api::list_users).post(users_api::add_user))
        .route("/admin/setup-status", get(users_api::setup_status))
        .route("/admin/auth/status", get(users_api::auth_status))
        .route("/admin/users/:id/approve", post(users_api::approve_user))
        .route("/admin/users/:id/reject", post(users_api::reject_user))
        .route("/admin/users/:id/groups", axum::routing::put(users_api::set_groups))
        .route("/admin/groups", get(groups_api::list_groups).post(groups_api::create_group))
        .route("/admin/groups/:id", axum::routing::put(groups_api::update_group).delete(groups_api::delete_group))
        .route("/admin/groups/:id/members", axum::routing::put(groups_api::set_members))
        .route("/admin/groups/:id/portfolios", axum::routing::put(groups_api::set_portfolios))
        .route("/admin/users/:id/admin", axum::routing::put(users_api::set_admin))
        .route("/admin/users/:id/disable", post(users_api::disable_user))
        .route("/admin/users/:id/enable", post(users_api::enable_user))
        .route("/admin/users/:id/keys", get(users_api::list_keys).post(users_api::create_key))
        .route("/admin/users/:id/keys/:key_id", axum::routing::delete(users_api::revoke_key))
        .route(
            "/admin/trading-tokens",
            post(admin::create_trading_token).get(admin::list_trading_tokens),
        )
        .route(
            "/admin/trading-tokens/:key_id",
            axum::routing::delete(admin::revoke_trading_token),
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
            "/admin/portfolios/:id/grants",
            get(grants::store::get_grants).put(grants::store::put_grants),
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
        .route("/admin/venues", get(admin::list_venues))
        .route(
            "/admin/broker-connections",
            post(admin::create_broker_connection).get(admin::list_broker_connections),
        )
        .route(
            "/admin/broker-connections/:code",
            axum::routing::patch(admin::update_broker_connection).get(admin::get_broker_connection),
        )
        .route(
            "/admin/broker-connections/:code/credentials",
            get(admin::get_broker_connection_credentials)
                .put(admin::put_broker_connection_credentials)
                .delete(admin::delete_broker_connection_credentials),
        )
        .route(
            "/admin/broker-connections/:code/credentials/test",
            post(admin::test_broker_connection_credentials),
        )
        .route("/admin/broker-connections/:code/universe", get(jobs::api::get_connection_universe))
        .route("/admin/broker-connections/:code/universe/load", post(jobs::api::load_connection_universe))
        .route("/admin/broker-connections/:code/universe/enrich", post(jobs::api::enrich_connection_universe))
        .route(
            "/admin/broker-connections/:code/recon",
            get(recon_admin_api::get_recon).patch(recon_admin_api::patch_recon),
        )
        .route(
            "/admin/broker-connections/:code/attribution-rules",
            get(recon_admin_api::list_rules).post(recon_admin_api::create_rule),
        )
        .route(
            "/admin/broker-connections/:code/attribution-rules/:id",
            axum::routing::put(recon_admin_api::update_rule).delete(recon_admin_api::delete_rule),
        )
        .route("/admin/broker-connections/:code/sweep", post(recon_admin_api::sweep))
        .route("/admin/recon-breaks", get(recon_admin_api::list_breaks))
        .route("/admin/recon-breaks/:id/adopt", post(recon_admin_api::adopt_break))
        .route("/admin/recon-breaks/:id/dismiss", post(recon_admin_api::dismiss_break))
        .route("/admin/jobs", get(jobs::api::list_jobs))
        .route("/admin/jobs/:id", get(jobs::api::get_job))
        .route("/admin/feed-connections", get(admin::list_feed_connections))
        .route(
            "/admin/feed-connections/:code/credentials",
            get(admin::get_feed_connection_credentials)
                .put(admin::put_feed_connection_credentials)
                .delete(admin::delete_feed_connection_credentials),
        )
        .route(
            "/admin/feed-connections/:code/credentials/test",
            post(admin::test_feed_connection_credentials),
        )
        .route("/admin/connections/reload", post(admin::reload_connections))
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
        .route(
            "/admin/strategies",
            post(strategies::admin::create_strategy).get(strategies::admin::list_strategies),
        )
        .route(
            "/admin/strategies/:code",
            axum::routing::patch(strategies::admin::update_strategy).delete(strategies::admin::delete_strategy),
        )
        .route(
            "/admin/strategies/:code/connections",
            axum::routing::put(strategies::admin::put_connections).get(strategies::admin::get_connections),
        )
        .route("/admin/strategies/:code/password", post(strategies::admin::reset_password))
        .route("/admin/strategies/:code/follow-credentials", axum::routing::put(strategies::admin::put_follow_credentials))
        .route("/admin/fix-acceptor/restart", post(strategies::admin::restart_acceptor))
        .route("/admin/strategies/:code/kill", post(strategies::admin::kill_strategy))
        .route("/admin/strategies/:code/enable", post(strategies::admin::enable_strategy))
        .route("/admin/strategies/:code/handover", post(strategies::admin::handover))
        .route("/admin/strategies/:code/rebaseline", post(strategies::admin::rebaseline_strategy))
        .route("/admin/strategies/:code/flatten", post(strategies::admin::flatten_strategy))
        .route(
            "/admin/strategies/:code/deployments",
            post(colo::deployment::register).get(colo::deployment::list),
        )
        .route("/admin/strategies/:code/deployments/:id", get(colo::deployment::get))
        .route(
            "/admin/strategies/:code/deployments/:id/retire",
            post(colo::deployment::retire),
        )
        .route(
            "/admin/strategies/:code/deployments/:id/verified-stop",
            post(colo::deployment::verified_stop),
        )
        .route(
            "/admin/strategies/:code/deployments/:id/adjustments",
            post(colo::ingest::adjustment),
        )
        .route("/admin/instruments", get(admin::list_instruments))
        .merge(instruments_import::routes())
        .route("/admin/feeds", get(admin::list_feeds))
        .route("/admin/symbology/resolve", post(admin::resolve_symbology))
        .route("/admin/symbology/backfill", post(admin::backfill_symbology))
        .route("/admin/instruments/expiry-sweep", post(admin::expiry_sweep))
        .layer(middleware::from_fn_with_state(state.clone(), auth::admin_middleware));

    let scalar_html = {
        let config = json!({ "url": "/api-docs/openapi.json" });
        scalar_api_reference::scalar_html_default(&config)
    };

    let mut app = Router::new()
        .route("/scalar", get(move || async move { Html(scalar_html) }))
        .route("/api-docs/openapi.json", get(|| async { axum::Json(ApiDoc::openapi()) }))
        // add health check route
        .route("/health", get(handlers::health))
        .merge(orders_router)
        .merge(admin_router)
        .merge(cockpit::router())
        // Reported strategies: bearer reporting token only, outside session auth.
        .merge(reported::api::router(state.pool().clone()));

    // 3) Register the unauthenticated OIDC routes — merged only when
    // `auth_api_state` resolved above, i.e. only when `[auth.oidc]` is
    // configured and discovery succeeded. Left unmerged, `/auth/*` falls
    // through to `handler_404` below exactly as it does with no OIDC config
    // at all: the off-by-default behaviour holds whether the block is
    // absent or merely not yet working.
    if let Some(auth_state) = auth_api_state.clone() {
        let auth_router = Router::new()
            .route("/auth/oidc/login", get(auth_api::login))
            .route("/auth/oidc/callback", get(auth_api::callback))
            // Alias: the registered IdP redirect URIs still use the pre-rename path.
            .route("/auth/callback", get(auth_api::callback))
            .layer(Extension(auth_state));
        app = app.merge(auth_router);
    }

    // Session routes. A session can be minted by local login or by OIDC, so
    // these are mounted whenever either is enabled — and, for OIDC, whenever
    // it is merely *configured*: discovery failing (IdP down, secret unset)
    // must not take `/auth/me` or `/auth/logout` away from sessions that
    // already exist. With neither enabled, the whole `/auth/*` surface 404s,
    // except `/auth/methods`, which the app's sign-in page reads to say so.
    //
    // `/auth/me` requires authentication, so it carries `auth_middleware`;
    // `/auth/logout` reads the session cookie itself and is idempotent.
    let oidc_discovered = auth_api_state.is_some();
    app = app.merge(local_auth_api::methods_routes(local_enabled, oidc_discovered));
    if local_enabled || oidc_configured {
        let local_auth = local_auth_api::LocalAuth::new(
            admin::resolve_master_key().ok().flatten(),
            local_enabled,
        );
        app = app.merge(local_auth_api::routes(local_auth));
        let me_router = Router::new()
            .route("/auth/me", get(auth_api::me))
            .route("/auth/keys", get(users_api::list_own_keys).post(users_api::create_own_key))
            .route("/auth/keys/:key_id", axum::routing::delete(users_api::revoke_own_key))
            .layer(middleware::from_fn_with_state(state.clone(), auth::auth_middleware));
        let logout_router = Router::new().route("/auth/logout", post(auth_api::logout));
        app = app.merge(me_router).merge(logout_router);
    }

    app
        // add 404 route as fallback
        .fallback(handlers::handler_404)
        .with_state(state)
}

// Server entry point (default when no subcommand is given).
async fn serve() {

    // No provisioning here. `oms database init` is the only thing that creates or
    // migrates a database, so starting the server can never mutate one.
    //
    let cfg = setup::database::config::resolve(Default::default());
    let role_password = setup::database::config::resolve_role_password(None);

    // A shipped default password is fine on a laptop and never anywhere else. Same
    // rule `database init` applies, asked of the same helper.
    if cfg.refuses_default_password(&role_password) {
        error!(
            "refusing to start: OMS_PASSWORD is still the built-in default against \
             non-loopback host {}",
            cfg.host
        );
        std::process::exit(1);
    }

    // The runtime pool is the `oms` role — the same one that owns the schema, and it
    // no longer has its own
    // host/port/database settings to drift from the ones init used.
    let runtime_url = cfg.runtime_url(&role_password);
    info!(
        "Connecting to {}:{}/{} as {}",
        cfg.host, cfg.port, cfg.database, setup::database::provision::ROLE
    );
    let pool = match PgPool::connect(&runtime_url).await {
        Ok(pool) => pool,
        Err(e) => {
            error!("Failed to connect to the database: {e}");
            // A connection failure with no config file found looks identical to
            // "never initialized" unless the message names where it was looked for.
            if config::load().is_some() {
                error!("config file: {}", config::path_abs().display());
            } else {
                let searched: Vec<_> = config::search_paths()
                    .into_iter()
                    .map(|p| config::absolute(p).display().to_string())
                    .collect();
                error!(
                    "no config file found; searched {} (set OMS_CONFIG to point elsewhere)",
                    searched.join(", ")
                );
            }
            error!("If this database has not been created yet, run: oms database init");
            return;
        }
    };

    // Master key + credential store. Resolved before anything else touches broker
    // or feed configuration, so a fatal key problem is reported before any other
    // setup work runs, and every consumer below (catalog auto-sync eligibility,
    // adapter registration, the feed gate) reads the same load rather than each
    // re-deriving its own view of "what is configured". `config::load()` is
    // memoized, so calling it again later in this function (for bind_addr /
    // admin_password) is cheap.
    let file_cfg = config::load();
    let master = match config::master_key(file_cfg) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => {
            error!("refusing to start: {e}");
            std::process::exit(1);
        }
        None => None,
    };

    // Routing config for every broker OMS natively supports — without a
    // broker_connection row a credential has nowhere to attach, whether or not
    // one is configured yet. Must run BEFORE `load_brokers` below: on a fresh
    // install this is what creates the alpaca-paper/binance-paper rows in the
    // first place, and `load_brokers` needs to see them to report them (even as
    // `Unconfigured`) rather than silently loading an empty list.
    setup::bootstrap::ensure_broker_connections(&pool).await;
    setup::bootstrap::ensure_feed_connections(&pool).await;

    // Any job still queued or running belonged to a process that is gone. Before
    // anything below can start a new one.
    match jobs::mark_interrupted(&pool).await {
        Ok(0) => {}
        Ok(n) => warn!("{n} background job(s) were interrupted by the last shutdown"),
        Err(e) => error!("could not mark interrupted jobs: {e}"),
    }

    // Every broker/feed connection, credentials decoded under `master` (or left
    // `Unconfigured`/`Error` when there is none — see `decode` in credentials.rs).
    // Loaded once, here, and reused by catalog auto-sync eligibility below, by
    // adapter registration, and by the Databento feed gate — one read of the
    // store, so nothing downstream can see a different answer than another part
    // of boot already acted on.
    //
    // A query failure here is fatal, not `unwrap_or_default()`'s empty `Vec`: an
    // empty list is indistinguishable from "nothing configured", so a transient
    // DB error at exactly this moment would silently produce a server that
    // binds, passes `/health`, and 503s every order with zero adapters
    // registered — precisely the misrepresented-state failure mode the
    // decrypt-gate just below exists to prevent, just reached a different way.
    let broker_connections = match credentials::load_brokers(&pool, master.as_ref()).await {
        Ok(c) => c,
        Err(e) => {
            error!("refusing to start: failed to load broker connections from the store: {e}");
            std::process::exit(1);
        }
    };
    let feed_connections = match credentials::load_feeds(&pool, master.as_ref()).await {
        Ok(c) => c,
        Err(e) => {
            error!("refusing to start: failed to load feed connections from the store: {e}");
            std::process::exit(1);
        }
    };

    // A master key that decrypts NONE of what is stored is fatal: starting
    // "successfully" with no adapters registered would misrepresent the system's
    // state — the same failure mode a missing key entirely produces (a missing
    // key decodes every row with a blob to this same `Error` state, see `decode`
    // in credentials.rs, so that case needs no separate check here). A key that
    // opens SOME rows but not others must still start: one bad or stale
    // credential must not be able to disarm every other one, so this only fires
    // when literally nothing usable came back — see `nothing_decrypted`.
    let any_configured = broker_connections.iter().any(|c| matches!(c.credentials, CredentialState::Configured(_)))
        || feed_connections.iter().any(|c| matches!(c.credentials, CredentialState::Configured(_)));
    let any_error = broker_connections.iter().any(|c| matches!(c.credentials, CredentialState::Error(_)))
        || feed_connections.iter().any(|c| matches!(c.credentials, CredentialState::Error(_)));
    if nothing_decrypted(any_configured, any_error) {
        // Named per-connection first, immediately above the summary, so an
        // operator sees exactly which rows failed rather than just the count.
        for conn in &broker_connections {
            if let CredentialState::Error(e) = &conn.credentials {
                error!(code = %conn.code, "credentials unusable: {e}");
            }
        }
        for conn in &feed_connections {
            if let CredentialState::Error(e) = &conn.credentials {
                error!(code = %conn.code, "credentials unusable: {e}");
            }
        }
        error!(
            "refusing to start: credentials are stored but none of them could be decrypted \
             under the configured master key (see the per-connection errors above). The likely \
             cause is that oms.master_key (or OMS_MASTER_KEY) does not match the key these \
             credentials were sealed with. Restore the correct key in oms.toml, or — if it is \
             truly gone — re-import/re-enter the credentials under the current key."
        );
        std::process::exit(1);
    }

    // Which of those brokers the *store* currently credentials, and of those, whose
    // instrument universe was never loaded — computed once so preflight (below)
    // and the boot loads (near the end) cannot disagree about who is eligible.
    let synced_brokers = setup::brokers::brokers_with_creds(&broker_connections);
    let boot_loads = setup::bootstrap::pending_boot_loads(&pool, &synced_brokers).await;
    let boot_pool = pool.clone();

    // Refuse to start on a catalog that cannot work, and name what is merely
    // degraded. Before any feed spawns, so a broken catalog surfaces here rather
    // than as a feed that quietly subscribes to nothing. An empty catalog is not
    // fatal when a background load is about to fill it.
    let auto_sync_pending = !boot_loads.is_empty();
    if let Err(e) = preflight::run(&pool, auto_sync_pending).await {
        error!("preflight failed: {e}");
        return;
    }

    // Init kafka client from .env (optional — publishing is disabled if not configured).
    // The config is kept alongside the client for the outbox bridge's topic names.
    let kafka_setup: Option<(kafka::KafkaConfig, kafka::KafkaClient)> = match kafka::KafkaConfig::from_env() {
        Ok(cfg) => match cfg.create_producer_client() {
            Ok(client) => { info!("Kafka producer ready (topic: {})", cfg.orders_topic); Some((cfg, client)) }
            Err(err) => { warn!("Kafka producer failed, publishing disabled: {}", err); None }
        },
        Err(err) => { info!("Kafka not configured ({}), publishing disabled", err); None }
    };


    // `file_cfg` was already loaded above (for the master key); `config::load()`
    // is memoized, so re-deriving `bind_addr` here — where the admin-password rule
    // below needs to know whether we are about to expose the server beyond this
    // machine — costs nothing extra.
    let bind_addr = resolve_bind_addr(env::var("OMS_BIND_ADDR").ok(), file_cfg);

    if env::var_os("OMS_ADMIN_PASSWORD").is_some() || env::var_os("OMS_ADMIN_TOKEN").is_some() {
        warn!("OMS_ADMIN_PASSWORD / OMS_ADMIN_TOKEN are no longer used and are ignored; sign in as a named admin");
    }

    // Same rule, for OIDC login instead of the admin console. Plain http is
    // only tolerated when the whole deployment is loopback — see
    // `refuses_plain_http_oidc` for why both halves have to be asked.
    if let Some(oidc) = file_cfg.and_then(|f| f.oidc()) {
        if refuses_plain_http_oidc(&bind_addr, &oidc.public_base_url) {
            error!(
                "refusing to start: [auth.oidc] is configured and this is not a purely local \
                 deployment — OMS_BIND_ADDR is {bind_addr} and auth.oidc.public_base_url is {} \
                 (not https://). The session cookie would either be sent in clear text or be \
                 dropped by the browser for carrying Secure over http. Serve OIDC login behind \
                 https, or keep both the bind and the public base URL on loopback.",
                oidc.public_base_url
            );
            std::process::exit(1);
        }
        if refuses_dev_client_secret(&bind_addr, env::var("OMS_OIDC_CLIENT_SECRET").ok().as_deref()) {
            error!(
                "refusing to start: OMS_OIDC_CLIENT_SECRET is the public development secret and \
                 OMS_BIND_ADDR ({bind_addr}) is not loopback. Register a real client secret at \
                 your identity provider and set OMS_OIDC_CLIENT_SECRET to it."
            );
            std::process::exit(1);
        }
    }

    // Stream health + the event bus are created here (before the broker
    // registry) because FIX sessions register their adapter *and* start their
    // session in one step, so they need both up front. The same StreamHealthRegistry
    // is handed to AppState so REST/WS streams spawned later share it.
    let stream_health = gateway::stream_health::StreamHealthRegistry::new();

    // The in-process event bus (see `event_bus.rs`). The fill path publishes
    // committed events and position changes on its order lane; market-data feeds
    // subscribe to it to re-read the held set. One bus for the whole process,
    // held by `AppState` so a reload hands restarted streams the same one.
    let event_bus = event_bus::EventBus::new();

    // Rebuild an empty position projection from the event log before anything
    // that can book a fill starts: the order engine, colo ingest, the FIX
    // acceptor, exec streams and reconcilers all start below. Migration 0067
    // empties it so every row carries its trade-time key. No-op on a fresh install.
    match positions::rebuild_if_empty(&pool).await {
        Ok(Some(n)) => info!(positions = n, "rebuilt position projection from event log"),
        Ok(None) => {}
        // Refuse to start: booking into an empty projection would leave positions
        // without trade-time keys, and no later boot would rebuild a non-empty table.
        Err(e) => {
            error!(error = ?e, "failed to rebuild position projection; refusing to start");
            std::process::exit(1);
        }
    }

    // The order engine: the only place that books. Connectors send it execution
    // events instead of writing to the database (see `order_engine.rs`). Started
    // before any connector, though its channel would buffer either way.
    let (engine, engine_rx) = gateway::contract::EngineHandle::new();
    tokio::spawn(order_engine::run(engine_rx, pool.clone(), event_bus.clone()));

    // The market data gateway, and the one receiver of every quote it emits.
    // Created here, ahead of `AppState`, for the same reason: a runtime reload
    // restarts a credentialed feed on this exact gateway, so its quotes reach
    // the one mark router boot started.
    let (market_data, quote_rx) = gateway::md::MarketDataGateway::new(stream_health.clone());

    // Adapters come from the store, not the environment. `build_registry` is the
    // one code path that turns a credential into an adapter, so a credential
    // saved at runtime (a later task's reload endpoint) and one loaded at boot
    // cannot diverge — boot is simply its first caller, with `is_boot = true`
    // and an empty registry (nothing yet exists to carry forward).
    // `broker_connections` was loaded once, above, alongside the master-key
    // resolution.
    let registration_deps = RegistrationDeps {
        stream_health: stream_health.clone(),
        engine: engine.clone(),
    };
    let RegistrationOutput {
        registry,
        // Not consumed yet — Task 5's reload endpoint returns this as the HTTP
        // response body. Every connection's outcome is already logged above by
        // `build_registry`, so there is nothing left to do with it here.
        report: _report,
        exec_streams,
        daily_stats,
    } = reload::build_registry(&broker_connections, true, &BrokerRegistry::new(), &DailyStatsRegistry::new(), &registration_deps).await;

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

    // AppState. Constructed with an empty registry and immediately swapped to the
    // one `build_registry` just produced — the same `swap_registries` call a
    // runtime reload will use — so boot itself exercises that path rather than
    // being the one caller that bypasses it. `event_bus` and the market data
    // gateway are handed in (not just kept as `serve()` locals) so the
    // `/admin/connections/reload` handler can rebuild an equivalent
    // `RegistrationDeps` and restart feeds on the same gateway boot used.
    // Session cookie policy is derived once here from the OIDC public base URL
    // — the browser's own view of this server, so a TLS terminator in front of
    // a loopback bind still gets `Secure` and the `__Host-` prefix — falling
    // back to the bind address when OIDC is off (see `sessions::cookie_policy`)
    // rather than recomputed on every request.
    // `ttl`/`public_base_url` come from `[auth.oidc]` when it's configured —
    // `FileConfig::oidc` is the one place idle/absolute TTL overrides and the
    // public base URL are read — and fall back to `SessionTtl::default()`/
    // `None` when it is not. A session can only ever be minted by the OIDC
    // callback, so `public_base_url` being `None` here means OIDC is off,
    // which means no session-authenticated request can ever reach
    // `auth_middleware`'s CSRF origin check to begin with — see that
    // function's doc comment for why a missing base URL still fails closed
    // there rather than assuming this.
    let oidc_settings = file_cfg.and_then(|f| f.oidc());
    if let Some(f) = file_cfg {
        f.warn_deprecated_auth();
    }
    // Built from `[auth]`/`[auth.session]` whether or not OIDC is configured:
    // local login mints sessions too.
    let (auth_settings, local_enabled) = match file_cfg {
        Some(f) => {
            let a = f.auth_settings();
            let local = a.local_enabled;
            (a, local)
        }
        None => (config::FileConfig::default().auth_settings(), true),
    };
    // The CSRF expected origin must be configured, never taken from a request.
    // Unset on a loopback bind it is derived from the bind address; unset
    // anywhere else, local login has no safe origin to check, so refuse.
    let public_base_url = match auth_settings.public_base_url {
        Some(url) => Some(url),
        None if local_enabled => match sessions::derive_public_base_url(&bind_addr) {
            Ok(url) => Some(url),
            Err(msg) => {
                error!("{msg}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let session_config = sessions::SessionConfig {
        cookie_policy: sessions::cookie_policy(&bind_addr, public_base_url.as_deref()),
        ttl: auth_settings.ttl,
        public_base_url,
    };

    let mut state = AppState::new(
        pool,
        BrokerRegistry::new(),
        symbology,
        stream_health,
        event_bus,
        engine,
        std::sync::Arc::new(market_data),
        session_config,
    );
    state.require_mfa = auth_settings.require_mfa;
    state.signup = accounts::SignupPolicy {
        enabled: auth_settings.enable_signup,
        pending_by_default: auth_settings.default_user_role == config::DefaultUserRole::Pending,
    };
    // A fresh host token every boot, for `oms` CLI commands on this machine.
    let token_path = admin_bootstrap::host_token_path();
    match admin_bootstrap::write_host_token(&token_path) {
        Ok(hash) => state.host_token_hash = Some(hash),
        Err(e) => warn!(path = %token_path.display(), error = %e, "could not write the host token; host CLI admin calls will be refused"),
    }
    state.swap_registries(registry, daily_stats);
    if let Err(e) = state.strategies().reload(state.pool()).await {
        error!(error = %e, "strategies: could not load the registry; no strategy can log on or trade");
    }
    // Co-located strategies: consume what their follow streams report, then
    // start following their accounts.
    {
        let (hub, s) = (state.colo().clone(), state.clone());
        tokio::spawn(async move { hub.run(s).await });
    }
    tokio::spawn(colo::control::positions_loop(state.clone()));
    tokio::spawn(colo::control::sweep_loop(state.clone()));
    tokio::spawn(colo::ingest::drain_loop(state.clone()));
    colo::follow::sync_follows(&state).await;
    match file_cfg.and_then(|f| f.control_link()) {
        Some(settings) => {
            tokio::spawn(colo::link_server::serve(state.clone(), settings));
        }
        None => info!("control link: not configured"),
    }

    // Before the acceptor opens: a strategy that reconnects at once must not
    // have its new orders taken for leftovers.
    strategies::control::boot_recovery(&state).await;
    if let Err(e) = state.fix_sessions().start(&state, file_cfg.and_then(|f| f.fix_acceptor())) {
        error!(error = %e, "FIX acceptor did not start; strategies cannot log on over FIX");
    }

    // (stream_health, event_bus and the market data gateway were created
    // before the broker registry so FIX sessions could use them, and before
    // `AppState` so it could be constructed holding them.)

    // Start every registered connection's execution stream — the same call a
    // runtime reload makes (`reload::restart_exec_stream`), so boot and reload
    // cannot drift apart. Each is registered in `StreamRegistry` under
    // `exec_stream_code`, so a later credential swap can abort and respawn just
    // that task rather than leave fills arriving against a credential orders no
    // longer route on.
    for stream in &exec_streams {
        reload::restart_exec_stream(stream, &registration_deps, state.streams(), state.pool()).await;
    }

    // Market data: feeds emit quotes onto one channel (created above, alongside
    // `AppState`); the router is the sole writer to MarkStore. Adding a vendor
    // means spawning another feed here — nothing downstream changes.
    tokio::spawn(mark_router::run(quote_rx, state.marks().clone(), state.pool().clone(), state.event_bus().clone()));

    // Kafka bridge: publishes committed order events from the outbox, catching up
    // after an outage. Without Kafka, events still queue; say how many wait.
    match kafka_setup {
        Some((cfg, client)) => {
            tokio::spawn(kafka_outbox::run(
                state.pool().clone(),
                std::sync::Arc::new(kafka::KafkaSink(client)),
                state.event_bus().subscribe_order(),
                cfg.topics(),
            ));
        }
        None => match kafka_outbox::pending_count(state.pool()).await {
            Ok(n) if n > 0 => info!("kafka outbox: {n} events waiting; Kafka not configured"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "kafka outbox: could not count pending events"),
        },
    }

    // Periodically refresh previous-close data for the interesting set (held
    // positions + watchlist) so Day P&L has a baseline even before a live quote
    // arrives. See `daily_stats_poller` for cadence and selection.
    tokio::spawn(daily_stats_poller::run(
        state.pool().clone(),
        state.daily_stats_handle(),
        state.daily_stats().clone(),
    ));

    // Retire dated contracts once their expiry instant passes, so the feeds below
    // stop resubscribing to them and the order path stops accepting them. Not
    // supervised: `stream_supervisor` exists to reconnect streams, and treats a clean
    // return as a disconnect to back off from — wrong shape for a periodic job.
    tokio::spawn(expiry::run(state.pool().clone()));
    tokio::spawn(jobs::eod_sweep::run(state.pool().clone()));

    // Re-resolve imported IBKR instruments daily at 06:00 UTC while a TWS session is live.
    tokio::spawn(instruments_import::run_nightly(state.pool().clone()));

    // Delete browser sessions past their absolute cap. Idle expiry is already
    // enforced on read (`sessions::lookup_session`'s join to `principal`), so
    // this sweep only clears rows that can never resolve again — it exists to
    // keep `user_session` from growing forever, not to enforce expiry itself.
    // Hourly, same cadence as the expiry sweep above, for the same reason: the
    // work matches almost nothing on a normal tick, so a shorter interval buys
    // nothing and a longer one just leaves dead rows around longer.
    {
        let pool = state.pool().clone();
        tokio::spawn(async move {
            loop {
                match sessions::sweep_expired(&pool).await {
                    Ok(0) => {}
                    Ok(removed) => info!(removed, "session sweep"),
                    Err(e) => error!(error = %e, "session sweep failed; retrying next interval"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

    // Feed credentials come from the store, same as brokers above. Gating the
    // spawn here is not enough on its own — `DatabentoOpraFeed` takes the key
    // explicitly and passes it to the `databento` client's `.key(...)` builder, so
    // nothing downstream falls back to reading `DATABENTO_API_KEY` itself.
    for conn in &feed_connections {
        if conn.status != "ACTIVE" {
            info!(code = %conn.code, "feed connection disabled, skipping");
            continue;
        }
        match &conn.credentials {
            CredentialState::Unconfigured => {
                info!(code = %conn.code, "no credentials stored, feed not started");
            }
            CredentialState::Error(e) => {
                error!(code = %conn.code, "credentials unusable: {e}");
            }
            CredentialState::Configured(FeedCredentials::Databento { api_key }) => {
                // `DatabentoOpraFeed` is hardcoded to one dataset (OPRA.PILLAR) and
                // one shared stream-health label ("DATABENTO"/"OPRA") — spawning it
                // for more than one row would have two sessions silently racing to
                // subscribe under the same health handle. `code` is the table's
                // primary key, so it cannot duplicate; guarding on it here instead
                // rejects a *different* Databento-provider row (e.g. a future
                // non-OPRA dataset reusing this credential shape) rather than
                // spawning an OPRA session for something that isn't OPRA.
                if conn.code != "databento-opra" {
                    error!(
                        code = %conn.code,
                        "unsupported Databento feed connection — only databento-opra \
                         (OPRA options) is implemented; feed not started"
                    );
                    continue;
                }
                // Credential-driven, so its task is registered in `StreamRegistry`
                // (unlike the public Binance/Bybit feeds below): a later credential
                // change can abort and replace it without a process restart. Boot
                // exercises the exact same call a runtime reload will use.
                reload::restart_databento_feed(
                    api_key.clone(),
                    state.market_data(),
                    state.stream_health(),
                    state.streams(),
                ).await;
                info!(code = %conn.code, "registered DATABENTO/OPRA feed");
            }
        }
    }

    // Public market data: every connector whose venue has a public quote feed
    // runs it on the market data gateway — no credentials, so it is always on,
    // for the life of the process. What they price is what is held: the core
    // keeps the gateway's subscriptions in step with the position table.
    state.market_data().start_public_feeds();
    tokio::spawn(market_data::run(state.pool().clone(), state.market_data().handle(), state.event_bus().subscribe_order()));

    // Load each credentialed broker's never-loaded instrument universe as a
    // background job, after everything above so the load never delays the server
    // binding below. `boot_loads` was computed before preflight.
    for broker in boot_loads {
        match jobs::universe::spawn_universe_load(boot_pool.clone(), broker, "system").await {
            Ok(id) => info!("boot: loading {} instrument universe (job {id})", broker.code()),
            Err(e) => warn!("boot: {} instrument universe not loaded: {e}", broker.code()),
        }
    }

    // OIDC login. `config.oidc()` is `None` unless `[auth.oidc]` has every
    // required field (see `FileConfig::oidc`) — that, alone, is what keeps
    // login off by default: no block means `auth_api_state` stays `None`,
    // `auth_router` below is never built, and `/auth/*` falls through to
    // `handler_404` exactly as it does today.
    //
    // The client secret is deliberately not read from `oms.toml` (nor from
    // the sealed broker/feed credential store `credentials.rs` uses — see
    // this task's report for why that store doesn't fit a single, singleton
    // secret without new schema and a new write path that is out of this
    // task's scope) — `OMS_OIDC_CLIENT_SECRET` is the bootstrap-tier
    // environment variable, the same tier `OMS_ADMIN_PASSWORD` and
    // `OMS_MASTER_KEY` already live on.
    //
    // A discovery failure (IdP unreachable, secret missing, or an insecure
    // endpoint refused by `oidc::Provider::discover`) logs an error and
    // leaves `auth_api_state` `None` rather than exiting: an operator who
    // configured `[auth.oidc]` still needs the rest of the OMS — order
    // routing, the admin console — to come up even if the IdP is down or the
    // secret hasn't been set yet. It is retried only on the next restart.
    let mut oidc_status = app_state::OidcStatus::default();
    let auth_api_state: Option<auth_api::AuthApiState> =
        if let Some(oidc_settings) = file_cfg.and_then(|f| f.oidc()) {
            oidc_status.configured = true;
            oidc_status.issuer = Some(oidc_settings.issuer.clone());
            oidc_status.redirect_uri =
                Some(format!("{}/auth/callback", oidc_settings.public_base_url.trim_end_matches('/')));
            match env::var("OMS_OIDC_CLIENT_SECRET").ok().filter(|v| !v.is_empty()) {
                None => {
                    error!(
                        "[auth.oidc] is configured but OMS_OIDC_CLIENT_SECRET is not set — OIDC \
                         login is disabled for this run. Set OMS_OIDC_CLIENT_SECRET to the client \
                         secret registered at the provider and restart."
                    );
                    oidc_status.error = Some("OMS_OIDC_CLIENT_SECRET is not set".to_string());
                    None
                }
                Some(client_secret) => {
                    let required_claim = oidc_settings.required_claim.clone();
                    let require_amr = oidc_settings.require_amr.clone();
                    let trust_email_verified = oidc_settings.trust_email_verified;
                    let enable_signup = oidc_settings.enable_signup;
                    let merge_accounts_by_email = oidc_settings.merge_accounts_by_email;
                    match oidc::Provider::discover(oidc_settings, client_secret).await {
                        Ok(provider) => {
                            info!("OIDC discovery succeeded — login enabled");
                            oidc_status.discovery_ok = true;
                            Some(auth_api::AuthApiState {
                                provider: Arc::new(provider),
                                required_claim,
                                require_amr,
                                trust_email_verified,
                                enable_signup,
                                merge_accounts_by_email,
                                flows: Arc::new(auth_api::FlowStore::default()),
                            })
                        }
                        Err(e) => {
                            error!(error = ?e, "OIDC discovery failed — login is disabled for this run");
                            oidc_status.error = Some(match e {
                                oidc::OidcError::ProviderUnavailable(msg) => msg,
                                other => format!("{other:?}"),
                            });
                            None
                        }
                    }
                }
            }
        } else {
            None
        };
    state.set_oidc_status(oidc_status);

    let app = build_app(state, auth_api_state, local_enabled, oidc_settings.is_some());

    // Start TCP listener
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    let host_url = format!("http://{}", bind_addr);
    info!("OMS listening on {}", host_url);
    if cockpit::is_bundled() {
        info!("UI: {}/app/", host_url);
    }
    info!("Scalar UI: {}/scalar", host_url);
    info!("OpenAPI spec: {}/api-docs/openapi.json", host_url);
    // Peer address for the login rate limiter (never X-Forwarded-For).
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
}

#[cfg(test)]
mod tests {
    /// axum panics while *building* a router that registers one (method, path)
    /// twice, so this fails at startup in production and here alike. Both OIDC
    /// branches are built so the conditionally-merged routers are covered too.
    #[tokio::test]
    async fn the_full_router_builds() {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let state = crate::handlers::tests::test_app_state(pool);
        let discovered = || crate::auth_api::AuthApiState {
            provider: std::sync::Arc::new(crate::oidc::Provider::for_test(
                "https://id.example.com",
                "oms",
                "https://oms.example.com",
            )),
            required_claim: None,
            require_amr: Vec::new(),
            trust_email_verified: true,
            enable_signup: false,
            merge_accounts_by_email: false,
            flows: std::sync::Arc::new(crate::auth_api::FlowStore::default()),
        };
        // (local only), (OIDC configured, discovery failed), (neither),
        // then the discovered router: (OIDC only) and (both).
        let _ = crate::build_app(state.clone(), None, true, false);
        let _ = crate::build_app(state.clone(), None, false, true);
        let _ = crate::build_app(state.clone(), None, false, false);
        let _ = crate::build_app(state.clone(), Some(discovered()), false, true);
        let _ = crate::build_app(state, Some(discovered()), true, true);
    }

    #[test]
    fn init_takes_the_guided_setup_flags() {
        use clap::Parser;
        use crate::{setup, Cli, Command, ConfigCmd};
        let cli = Cli::try_parse_from([
            "oms", "init", "--non-interactive", "--postgres", "docker",
            "--bind", "localhost:4000",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Init { non_interactive, postgres, bind, .. }) => {
                assert!(non_interactive);
                assert_eq!(postgres, Some(setup::init::rules::DbSource::Docker));
                assert_eq!(bind.as_deref(), Some("localhost:4000"));
            }
            _ => panic!("not init"),
        }
        assert!(Cli::try_parse_from(["oms", "init", "--postgres", "kubernetes"]).is_err());
        assert!(Cli::try_parse_from(["oms", "init", "--admin-password", "twelve-chars-ok"]).is_err(), "init no longer takes a password");
        assert!(Cli::try_parse_from(["oms", "init", "--admin-email", "ada@example.com"]).is_err(), "the first admin signs up in the cockpit");
    }

    #[test]
    fn config_path_is_a_command() {
        use clap::Parser;
        use crate::{setup, Cli, Command, ConfigCmd};
        let cli = Cli::try_parse_from(["oms", "config", "path"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Config(ConfigCmd::Path))));
    }
    use super::bind_is_loopback;
    use super::{nothing_decrypted, resolve_bind_addr, DEFAULT_BIND_ADDR};

    /// The most consequential new behaviour in the credential-store plan,
    /// table-tested directly: refuse iff at least one stored credential is
    /// unusable AND *none at all* decoded successfully. A missing master key
    /// is not a separate case — every row with a blob decodes to `Error` with
    /// no key at all, so it falls under "all fail" for free (last row).
    /// The load-bearing property this guards is the "some fail" row: a single
    /// bad or stale credential must never be able to disarm every other one.
    #[test]
    fn nothing_decrypted_only_when_nothing_at_all_decoded() {
        // (any_configured, any_error) -> must_refuse
        let cases = [
            (false, false, false), // none stored: normal fresh install
            (true, false, false),  // all decrypt: normal running system
            (true, true, false),   // some fail: one bad row must not disarm the rest
            (false, true, true),   // all fail (includes "no key configured" — see decode()): refuse
        ];
        for (any_configured, any_error, expected) in cases {
            assert_eq!(
                nothing_decrypted(any_configured, any_error),
                expected,
                "any_configured={any_configured} any_error={any_error}"
            );
        }
    }

    /// Generating the document is the only thing that would actually catch a
    /// bad `#[schema(...)]` override (e.g. on `SaveResponse::reload`, which
    /// points at a type — `reload::ConnectionOutcome` — with no `ToSchema`
    /// impl of its own) or a typo'd path: nothing else in the test suite
    /// calls `openapi()`.
    #[test]
    fn openapi_document_includes_the_credential_write_endpoints() {
        use utoipa::OpenApi;
        let doc = super::ApiDoc::openapi();
        let json = serde_json::to_string(&doc).expect("serialize openapi doc");
        assert!(json.contains("/admin/broker-connections/{code}/credentials"));
        assert!(json.contains("/admin/broker-connections/{code}/credentials/test"));
        assert!(json.contains("/admin/feed-connections"));
        assert!(json.contains("/admin/feed-connections/{code}/credentials"));
        assert!(json.contains("/admin/feed-connections/{code}/credentials/test"));
        assert!(json.contains("SaveResponse"));
        assert!(json.contains("TestResponse"));
        assert!(json.contains("CredentialSubmission"));
        assert!(json.contains("FeedConnectionSummary"));
    }


    /// Loopback binds, with or without a port.
    #[test]
    fn loopback_binds_are_recognised() {
        assert!(bind_is_loopback("localhost:3001"));
        assert!(bind_is_loopback("127.0.0.1:3001"));
        assert!(bind_is_loopback("[::1]:3001"));
        assert!(bind_is_loopback("localhost"));
    }

    /// Anything reachable from another machine must refuse the default password.
    /// `0.0.0.0` is the one that matters — it looks local and is not.
    #[test]
    fn exposed_binds_are_not_loopback() {
        assert!(!bind_is_loopback("0.0.0.0:3001"));
        assert!(!bind_is_loopback("192.168.1.10:3001"));
        assert!(!bind_is_loopback("oms.internal:3001"));
    }

    /// The OIDC start-up gate. Plain http is only ever acceptable when the
    /// whole deployment is loopback; `https://` is fine on any bind, which is
    /// the reverse-proxy topology C2 was about.
    #[test]
    fn boot_refuses_dev_secret_off_loopback() {
        use super::refuses_dev_client_secret as refuses;

        assert!(refuses("0.0.0.0:3001", Some("oms-dev-secret")));
        assert!(refuses("oms.internal:3001", Some("oms-dev-secret")));
        // Local development may keep the fixture's secret.
        assert!(!refuses("127.0.0.1:3001", Some("oms-dev-secret")));
        assert!(!refuses("localhost:3001", Some("oms-dev-secret")));
        // A real secret, or none, is not this rule's business.
        assert!(!refuses("0.0.0.0:3001", Some("a-real-registered-secret")));
        assert!(!refuses("0.0.0.0:3001", None));
    }

    #[test]
    fn plain_http_oidc_is_only_tolerated_when_everything_is_local() {
        use super::refuses_plain_http_oidc as refuses;

        // The reverse-proxy topology: loopback bind, TLS terminated in front.
        assert!(!refuses("127.0.0.1:3001", "https://oms.example.com"));
        assert!(!refuses("0.0.0.0:3001", "https://oms.example.com"));
        // Genuine local development, plain http end to end.
        assert!(!refuses("127.0.0.1:3001", "http://localhost:3001"));
        assert!(!refuses("[::1]:3001", "http://[::1]:3001"));

        // Reachable from elsewhere and not https: the original rule.
        assert!(refuses("0.0.0.0:3001", "http://oms.example.com"));
        // Served to the network but calling itself localhost — the cookie
        // would go out over the LAN with no Secure at all.
        assert!(refuses("0.0.0.0:3001", "http://localhost:3001"));
        // Loopback bind, public http base URL: the cookie demands Secure and
        // the browser drops it, so login would simply never work.
        assert!(refuses("127.0.0.1:3001", "http://oms.example.com"));
        // Neither scheme we understand.
        assert!(refuses("127.0.0.1:3001", "garbage"));
    }

    /// An address we cannot parse must fail closed, never open.
    #[test]
    fn unparseable_binds_are_not_loopback() {
        assert!(!bind_is_loopback("[::1:3001"), "unterminated IPv6 bracket");
        assert!(!bind_is_loopback(""));
    }

    #[test]
    fn bind_addr_prefers_env_then_file_then_default() {
        let file = crate::config::parse("[server]\nbind_addr = \"1.2.3.4:9999\"\n").expect("parse");

        assert_eq!(resolve_bind_addr(Some("0.0.0.0:1".into()), Some(&file)), "0.0.0.0:1");
        assert_eq!(resolve_bind_addr(None, Some(&file)), "1.2.3.4:9999");
        assert_eq!(resolve_bind_addr(None, None), DEFAULT_BIND_ADDR);
    }

    #[test]
    fn the_committed_openapi_spec_is_current() {
        // The docs site renders docs/openapi.json rather than calling a running
        // server, so a stale file silently publishes a wrong API reference.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json");
        let committed = std::fs::read_to_string(path)
            .expect("docs/openapi.json is missing — regenerate: cargo run -- openapi > docs/openapi.json");

        assert_eq!(
            committed.trim(),
            super::openapi_json().trim(),
            "docs/openapi.json is out of date. Regenerate it:\n\n    \
             cargo run -- openapi > docs/openapi.json\n"
        );
    }

    #[test]
    fn the_openapi_subcommand_renders_a_usable_spec() {
        // The docs site is built from this JSON, not from a running server, so the
        // command has to produce a complete spec with no database and no config.
        let json = super::openapi_json();
        let spec: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert!(spec["openapi"].as_str().is_some(), "missing openapi version");
        assert!(
            spec["paths"].as_object().map(|p| !p.is_empty()).unwrap_or(false),
            "spec has no paths"
        );
        assert!(
            spec["paths"]["/orders/submit"].is_object(),
            "expected /orders/submit in the spec, got: {:?}",
            spec["paths"].as_object().map(|p| p.keys().collect::<Vec<_>>())
        );
    }

    /// Every `$ref` in the spec must name a schema the spec defines. A dangling
    /// one (a type registered under a different name than the one fields refer
    /// to it by, or never registered) renders as "unknown" in Swagger UI and
    /// makes the docs site's API reference build fail outright.
    #[test]
    fn every_schema_ref_in_the_openapi_spec_resolves() {
        let spec: serde_json::Value = serde_json::from_str(&super::openapi_json()).unwrap();
        let defined: std::collections::BTreeSet<&str> = spec["components"]["schemas"]
            .as_object()
            .expect("components.schemas")
            .keys()
            .map(String::as_str)
            .collect();
        let text = spec.to_string();
        let dangling: std::collections::BTreeSet<&str> = text
            .split("\"#/components/schemas/")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .filter(|name| !defined.contains(name))
            .collect();
        assert!(dangling.is_empty(), "dangling schema refs: {dangling:?}");
    }

    /// Every route behind the MFA gate (order actions and every `/admin`
    /// mutation) documents its 403 `mfa_required`; reads do not.
    #[test]
    fn every_mfa_gated_route_documents_mfa_required() {
        let spec: serde_json::Value = serde_json::from_str(&super::openapi_json()).unwrap();
        let trading = [("post", "/orders/submit"), ("post", "/orders/cancel"), ("post", "/orders/{id}/allocations")];
        let mut missing = Vec::new();
        let mut gated = 0;
        for (path, ops) in spec["paths"].as_object().unwrap() {
            for (method, op) in ops.as_object().unwrap() {
                let is_gated = (path.starts_with("/admin/") && method != "get") || trading.contains(&(method.as_str(), path.as_str()));
                let documented = op["responses"]["403"]["description"].as_str().is_some_and(|d| d.contains("mfa_required"));
                if is_gated {
                    gated += 1;
                    if !documented {
                        missing.push(format!("{method} {path}"));
                    }
                } else {
                    assert!(!documented, "{method} {path} is not MFA-gated");
                }
            }
        }
        assert!(gated > 40, "{gated}");
        assert!(missing.is_empty(), "no 403 mfa_required on: {missing:?}");
    }

    /// The trading endpoints are what the public API reference leads with, and
    /// a missing summary titles the page by the Rust function name instead
    /// (`orders_submit`). A `///` doc comment on the handler supplies it.
    #[test]
    fn every_trading_operation_has_a_summary() {
        let spec: serde_json::Value = serde_json::from_str(&super::openapi_json()).unwrap();
        let mut missing = Vec::new();
        for (path, ops) in spec["paths"].as_object().unwrap() {
            for (method, op) in ops.as_object().unwrap() {
                let trading = op["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == "orders"));
                if trading && op["summary"].as_str().map_or(true, str::is_empty) {
                    missing.push(format!("{} {path}", method.to_uppercase()));
                }
            }
        }
        assert!(missing.is_empty(), "trading operations without a summary: {missing:?}");
    }

    /// Two handlers with the same function name get the same operationId, and
    /// the docs site's API reference then builds one page for both — the
    /// trader's `GET /portfolios` was published as the admin endpoint's page.
    #[test]
    fn every_operation_id_is_unique() {
        let spec: serde_json::Value = serde_json::from_str(&super::openapi_json()).unwrap();
        let mut seen = std::collections::BTreeMap::<String, String>::new();
        let mut dupes = Vec::new();
        for (path, ops) in spec["paths"].as_object().unwrap() {
            for (method, op) in ops.as_object().unwrap() {
                let id = op["operationId"].as_str().unwrap_or_default().to_string();
                let here = format!("{} {path}", method.to_uppercase());
                if let Some(first) = seen.insert(id.clone(), here.clone()) {
                    dupes.push(format!("{id}: {first} and {here}"));
                }
            }
        }
        assert!(dupes.is_empty(), "duplicate operationIds: {dupes:?}");
    }

    #[test]
    fn cli_reports_its_version() {
        use clap::CommandFactory;
        let cmd = <super::Cli as CommandFactory>::command();
        let version = cmd.get_version()
            .expect("--version must be available: install.sh smoke-checks it");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }
}
