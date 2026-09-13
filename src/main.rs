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
mod recon_orders;
mod symbology_resolver;
mod setup;

use crate::adapters::BrokerRegistry;
use crate::app_state::AppState;
use crate::credentials::{CredentialState, FeedCredentials};
use crate::reload::{RegistrationDeps, RegistrationOutput};
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
use tracing::{error, info, warn};
use tracing_subscriber::{self, EnvFilter};
use utoipa::OpenApi;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
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
mod bybit_feed;
mod feeds;
mod preflight;
mod config;
mod secrets;
mod credentials;
mod credentials_api;
mod expiry;
mod fix;
mod reload;
#[cfg(test)]
mod reload_tests;

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
        admin::create_trading_token,
        admin::list_trading_tokens,
        admin::revoke_trading_token,
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
        admin::get_broker_connection_credentials,
        admin::put_broker_connection_credentials,
        admin::delete_broker_connection_credentials,
        admin::test_broker_connection_credentials,
        admin::reload_connections,
        admin::create_risk_limit,
        admin::list_risk_limits,
        admin::get_risk_limit,
        admin::update_risk_limit,
        admin::delete_risk_limit,
        admin::list_instruments,
        admin::list_feeds,
        admin::resolve_symbology,
        admin::backfill_symbology,
        admin::expiry_sweep,
        admin::setup_status,
    ),
    components(schemas(
        SubmitOrder, SubmitOrderRequest, CancelOrder, OrderSide, OrderType, TimeInForce, OrderAggregateState,
        crate::positions::Position,
        Allocation, CreateAllocations, AllocationSplit, BlotterRow,
        handlers::GrantedPortfolio,
        Principal, Portfolio, Account, BrokerConnection,
        CreatePrincipal, UpdatePrincipal,
        CreatePortfolio, UpdatePortfolio,
        CreateAccount, UpdateAccount,
        CreateBrokerConnection, UpdateBrokerConnection,
        admin::RedactedCredentials, crate::credentials::RedactedField,
        crate::credentials_api::CredentialSubmission, admin::SaveResponse, admin::TestResponse,
        CreateKey, ApiKeyRecord,
        admin::CreateTradingToken, admin::TradingTokenCreated, admin::TradingTokenRow,
        Grant, CreateGrant, UpdateGrant,
        admin::RiskLimit, admin::CreateRiskLimit, admin::UpdateRiskLimit,
        admin::InstrumentSummary, admin::FeedSummary,
        admin::ResolveRequest, admin::BackfillRequest, admin::BackfillResult,
        admin::ExpirySweepResult,
        admin::SetupStatus, admin::SetupConnectionStatus, admin::SetupCatalogStatus,
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




/// Where the server listens when `OMS_BIND_ADDR` says nothing. Loopback by
/// design: a fresh clone should start and be reachable from a browser on the same
/// machine, and nothing more, until the operator says otherwise.
const DEFAULT_BIND_ADDR: &str = "localhost:3001";

/// Admin console password used when `OMS_ADMIN_PASSWORD` is unset *and* the bind
/// address is loopback. Mirrors `config::DEFAULT_ROLE_PASSWORD`: convenient on a
/// laptop, refused the moment the server is reachable from anywhere else.
const DEFAULT_ADMIN_PASSWORD: &str = "openoms-dev";

/// Is this bind address reachable only from this machine?
///
/// Splits the host off a `host:port` pair before asking, and treats anything it
/// cannot parse as non-loopback — an unrecognised address must not be what talks
/// the server into accepting a default password.
fn bind_is_loopback(addr: &str) -> bool {
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

/// Cockpit login password: env → file. `None` means unset, which the caller
/// turns into the loopback-only default or a refusal.
fn resolve_admin_password(
    from_env: Option<String>,
    file: Option<&config::FileConfig>,
) -> Option<String> {
    from_env
        .filter(|v| !v.is_empty())
        .or_else(|| file.and_then(|f| f.server.admin_password.clone()))
        .filter(|v| !v.is_empty())
}

/// Merges the two admin-password env vars for `resolve_admin_password`'s `from_env`
/// argument. `OMS_ADMIN_PASSWORD` must be emptiness-filtered *before* the
/// `or_else`, not after: `Option::or_else` only runs on `None`, so an unfiltered
/// `Some("")` from `OMS_ADMIN_PASSWORD=""` would short-circuit past a configured
/// `OMS_ADMIN_TOKEN` instead of falling through to it.
fn admin_password_from_env() -> Option<String> {
    env::var("OMS_ADMIN_PASSWORD")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| env::var("OMS_ADMIN_TOKEN").ok())
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
    /// Database provisioning and migration.
    #[command(subcommand)]
    Database(DatabaseCmd),
    /// Credential store maintenance.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// First-run setup: generate oms.toml and create the database.
    Init {
        /// Take values from flags and the environment instead of prompting.
        #[arg(long)]
        non_interactive: bool,
        #[command(flatten)]
        db: DbArgs,
    },
}

#[derive(clap::Subcommand)]
enum SetupCmd {
    /// Seed the master instrument catalog + broker_instrument mapping from a broker.
    SyncBroker(setup::brokers::Args),
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
    // `RUST_LOG` is honoured when set (e.g. `RUST_LOG=fix::wire=debug` to see
    // the redacted FIX wire log — see `fix::mod`'s doc comment); with no
    // `RUST_LOG` at all this falls back to plain `info`, so anyone who sets
    // nothing sees exactly what they saw before this filter existed.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = <Cli as clap::Parser>::parse();
    match cli.command {
        Some(Command::Setup(SetupCmd::SyncBroker(args))) => {
            if let Err(e) = setup::brokers::run(args).await {
                error!("setup sync-broker failed: {e}");
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
                    println!("rotated {n} credential(s) to a new master key.\n");
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
        Some(Command::Init { non_interactive, db }) => {
            // Checked here too, not only inside `run()`: `resolve()` below calls
            // `config::load()`, which exits the process over a *malformed*
            // oms.toml before `run()`'s own check ever runs — and interactively,
            // without this, the operator would type their superuser password at
            // a no-echo prompt only to be refused a moment later. `run()` keeps
            // its own check regardless; that one is the actual guarantee, this
            // one just fails earlier and more kindly.
            let cfg_path = config::path();
            if cfg_path.exists() {
                eprintln!("{}", setup::init::already_initialized_message(&cfg_path));
                std::process::exit(1);
            }

            let interactive = !non_interactive;
            let prompts = if non_interactive {
                let cfg = setup::database::config::resolve(db.into());
                setup::init::Prompts {
                    host: cfg.host,
                    port: cfg.port,
                    database: cfg.database,
                    username: cfg.username,
                    password: cfg.password,
                }
            } else {
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
                let password_override = db.password.clone();
                let cfg = setup::database::config::resolve(db.into());
                let defaults = setup::init::PromptDefaults {
                    host: cfg.host,
                    port: cfg.port,
                    database: cfg.database,
                    username: cfg.username,
                    password: password_override,
                };
                match setup::init::prompt(&defaults) {
                    Ok(p) => p,
                    Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
                }
            };
            if let Err(e) = setup::init::run(prompts, interactive).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        None => serve().await,
    }
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
            // `oms.toml` is resolved relative to the CWD, so a connection failure
            // from a directory other than the one it was written in looks
            // identical to "never initialized" unless the message names the path
            // that was actually searched.
            error!(
                "config file searched: {} (set OMS_CONFIG to point elsewhere)",
                config::path_abs().display()
            );
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

    // Which of those brokers the *store* currently credentials — computed once so
    // `will_sync_on_boot` (below) and `spawn_sync` (near the end, once the catalog
    // is confirmed empty) cannot disagree about who is eligible.
    let synced_brokers = setup::brokers::brokers_with_creds(&broker_connections);

    // Refuse to start on a catalog that cannot work, and name what is merely
    // degraded. Before any feed spawns, so a broken catalog surfaces here rather
    // than as a feed that quietly subscribes to nothing. An empty catalog is not
    // fatal when a background sync is about to fill it.
    let auto_sync_pending = setup::bootstrap::will_sync_on_boot(&pool, &synced_brokers).await;
    if let Err(e) = preflight::run(&pool, auto_sync_pending).await {
        error!("preflight failed: {e}");
        return;
    }

    // Init kafka client from .env (optional — publishing is disabled if not configured)
    let kafka_client: Option<kafka::KafkaClient> = match kafka::KafkaConfig::from_env() {
        Ok(cfg) => match cfg.create_producer_client() {
            Ok(client) => { info!("Kafka producer ready (topic: {})", cfg.orders_topic); Some(client) }
            Err(err) => { warn!("Kafka producer failed, publishing disabled: {}", err); None }
        },
        Err(err) => { info!("Kafka not configured ({}), publishing disabled", err); None }
    };


    // `file_cfg` was already loaded above (for the master key); `config::load()`
    // is memoized, so re-deriving `bind_addr` here — where the admin-password rule
    // below needs to know whether we are about to expose the server beyond this
    // machine — costs nothing extra.
    let bind_addr = resolve_bind_addr(env::var("OMS_BIND_ADDR").ok(), file_cfg);

    let admin_auth_enabled = env::var("OMS_ADMIN_AUTH_ENABLED")
        .map(|v| v.to_lowercase() != "false")
        .unwrap_or(true);

    // The admin console login password. `OMS_ADMIN_PASSWORD` is the canonical name;
    // `OMS_ADMIN_TOKEN` is still accepted for back-compat.
    //
    // Unset is not fatal on a loopback bind: a fresh clone must be able to run
    // `cargo run` and reach the console, the same way `database init` works with no
    // configuration at all. The moment the bind address is reachable from anywhere
    // else, the default is refused instead — identical to the role-password rule.
    let admin_token = if !admin_auth_enabled {
        String::new()
    } else {
        let configured = resolve_admin_password(admin_password_from_env(), file_cfg);
        match configured {
            Some(token) => token,
            None if bind_is_loopback(&bind_addr) => {
                warn!(
                    "OMS_ADMIN_PASSWORD is not set — using the built-in default \
                     '{DEFAULT_ADMIN_PASSWORD}' for the admin console. Set OMS_ADMIN_PASSWORD \
                     in .env before binding anywhere but localhost."
                );
                DEFAULT_ADMIN_PASSWORD.to_string()
            }
            None => {
                error!(
                    "refusing to start: OMS_ADMIN_PASSWORD is not set and OMS_BIND_ADDR \
                     ({bind_addr}) is reachable beyond this machine. Set OMS_ADMIN_PASSWORD \
                     in .env, or set OMS_ADMIN_AUTH_ENABLED=false to disable the console login."
                );
                std::process::exit(1);
            }
        }
    };

    // Stream health + the fill→marks doorbell are created here (before the broker
    // registry) because FIX sessions register their adapter *and* start their
    // session in one step, so they need both up front. The same StreamHealthRegistry
    // is handed to AppState so REST/WS streams spawned later share it.
    let stream_health = stream_health::StreamHealthRegistry::new();

    // Doorbell from the fill path to the marks feeds: a fill moved a position, so
    // re-read the held set. bounded(1) — a queued signal already means "reload",
    // so extras are redundant and try_send never blocks the fill path. A fan-out
    // task (spawned below, once every feed has registered) relays to each feed's
    // own doorbell, so execution.rs need not know how many feeds exist.
    //
    // The registry that fan-out reads from (keyed by feed code, so a restarted
    // feed's fresh sender replaces its old one rather than accumulating a dead
    // entry — see `DoorbellRegistry`'s doc comment) lives on `AppState` itself,
    // not as a local here: the `/admin/connections/reload` handler needs to
    // reach the exact same map this fan-out task drains, and a `serve()`-local
    // would be unreachable from a request handler.
    let (position_changed_tx, mut position_changed_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Quote channel every market-data feed publishes onto. Created here, ahead
    // of `AppState`, for the same reason: a runtime reload needs to hand a
    // restarted feed this exact sender, not a disconnected second channel, so
    // it has to be a value `AppState` can hold and hand back out, not a
    // `serve()`-local threaded only through the boot-time spawns below.
    let (quote_tx, quote_rx) = tokio::sync::mpsc::channel::<dataprovider::Quote>(1024);

    // Adapters come from the store, not the environment. `build_registry` is the
    // one code path that turns a credential into an adapter, so a credential
    // saved at runtime (a later task's reload endpoint) and one loaded at boot
    // cannot diverge — boot is simply its first caller, with `is_boot = true`
    // and an empty registry (nothing yet exists to carry forward).
    // `broker_connections` was loaded once, above, alongside the master-key
    // resolution.
    let registration_deps = RegistrationDeps {
        pool: pool.clone(),
        stream_health: stream_health.clone(),
        kafka: kafka_client.clone(),
        position_changed_tx: Some(position_changed_tx.clone()),
    };
    let RegistrationOutput {
        registry,
        // Not consumed yet — Task 5's reload endpoint returns this as the HTTP
        // response body. Every connection's outcome is already logged above by
        // `build_registry`, so there is nothing left to do with it here.
        report: _report,
        alpaca_creds,
        binance_rest_adapters,
    } = reload::build_registry(&broker_connections, true, &BrokerRegistry::new(), &registration_deps).await;

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
    // one `build_registry` just produced — the same `swap_registry` call a
    // runtime reload will use — so boot itself exercises that path rather than
    // being the one caller that bypasses it. `position_changed_tx` and
    // `quote_tx` are handed in (not just kept as `serve()` locals) so the
    // `/admin/connections/reload` handler can rebuild an equivalent
    // `RegistrationDeps` and restart feeds against the same channels boot used.
    let state = AppState::new(
        pool,
        admin_token,
        admin_auth_enabled,
        BrokerRegistry::new(),
        kafka_client,
        symbology,
        stream_health,
        Some(position_changed_tx.clone()),
        quote_tx.clone(),
    );
    state.swap_registry(registry);

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

    // (stream_health, position_changed_tx/rx and quote_tx/rx were created
    // before the broker registry so FIX sessions could use them, and before
    // `AppState` so it could be constructed holding them.)

    // Spawn Alpaca trade-update stream tasks (one per configured environment),
    // registered in `StreamRegistry` under a distinct key
    // (`reload::restart_alpaca_stream`'s `alpaca_exec_stream_code`) so a later
    // credential swap can abort and respawn just this task rather than leaving
    // fills arriving against a credential orders no longer route on — the same
    // split Plan 2 closed at boot, reintroduced at reload without this. Boot
    // exercises the exact call a runtime reload will use, same as
    // `restart_databento_feed` below. Credentials come from `alpaca_creds`,
    // stashed when the adapter was registered above — a second read of the
    // store (let alone the environment) here could in principle see a
    // different answer than what was just registered; reusing the same values
    // makes that impossible by construction.
    for env_name in ["PAPER", "LIVE"] {
        if let (Some((key, secret)), Some(adapter)) = (alpaca_creds.get(env_name), state.registry().get_alpaca(env_name)) {
            reload::restart_alpaca_stream(env_name, key.clone(), secret.clone(), adapter, &registration_deps, state.streams()).await;
        }
    }

    // Spawn the Binance user-data stream for every REST-transport adapter
    // registered above (`binance_stream::run` needs no credentials of its own —
    // the adapter already holds them).
    for (env_name, adapter) in binance_rest_adapters {
        let health = state.stream_health().handle("BINANCE", env_name, stream_health::StreamKind::Execution);
        let handle = tokio::spawn(binance_stream::run(env_name, state.pool().clone(), state.kafka().cloned(), adapter, health, Some(position_changed_tx.clone())));
        state.streams().insert(reload::binance_exec_stream_code(env_name), handle);
    }

    // Market data: feeds emit quotes onto one channel (created above, alongside
    // `AppState`); the router is the sole writer to MarkStore. Adding a vendor
    // means spawning another feed here — nothing downstream changes.
    tokio::spawn(mark_router::run(quote_rx, state.marks().clone(), state.pool().clone()));

    // Retire dated contracts once their expiry instant passes, so the feeds below
    // stop resubscribing to them and the order path stops accepting them. Not
    // supervised: `stream_supervisor` exists to reconnect streams, and treats a clean
    // return as a disconnect to back off from — wrong shape for a periodic job.
    tokio::spawn(expiry::run(state.pool().clone()));

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
                // and its doorbell in `DoorbellRegistry` (unlike the public
                // Binance/Bybit feeds below): a later credential change can abort
                // and replace it without a process restart. `restart_databento_feed`
                // owns its own doorbell channel pair — see its doc comment — so
                // boot exercises the exact same call a runtime reload will use.
                reload::restart_databento_feed(
                    api_key.clone(),
                    state.pool().clone(),
                    state.stream_health(),
                    state.streams(),
                    state.doorbells(),
                    quote_tx.clone(),
                ).await;
                info!(code = %conn.code, "registered DATABENTO/OPRA feed");
            }
        }
    }

    // Binance public market data — no credentials, so it is always on. Each feed
    // needs its own doorbell receiver (an mpsc has exactly one consumer), so the
    // sender is cloned per feed rather than shared. Not registered in
    // `StreamRegistry`: there is no credential that could change under it, so
    // there is nothing for a reload to restart here — leave it running for the
    // life of the process, same as before this feature existed.
    {
        let (binance_pos_tx, binance_pos_rx) = tokio::sync::mpsc::channel::<()>(1);
        state.doorbells().set("BINANCE/SPOT", binance_pos_tx);
        let health = state.stream_health().handle("BINANCE", "SPOT", stream_health::StreamKind::Feed);
        let session = quote_feed::QuoteFeedSession::new(
            binance_feed::BinanceFeed,
            state.pool().clone(),
            quote_tx.clone(),
            binance_pos_rx,
            health.clone(),
        );
        tokio::spawn(stream_supervisor::supervise("BINANCE/SPOT", health, session));
    }

    // Bybit public market data — a second source for the same crypto pairs, so a
    // Binance outage does not leave positions unmarked. Ranked below Binance in
    // provider_feed_policy; the router decides which one owns the mark. Also not
    // credential-driven, so — like Binance above — not registered in
    // `StreamRegistry`.
    {
        let (bybit_pos_tx, bybit_pos_rx) = tokio::sync::mpsc::channel::<()>(1);
        state.doorbells().set("BYBIT/SPOT", bybit_pos_tx);
        let health = state.stream_health().handle("BYBIT", "SPOT", stream_health::StreamKind::Feed);
        let session = quote_feed::QuoteFeedSession::new(
            bybit_feed::BybitFeed,
            state.pool().clone(),
            quote_tx.clone(),
            bybit_pos_rx,
            health.clone(),
        );
        tokio::spawn(stream_supervisor::supervise("BYBIT/SPOT", health, session));
    }

    // Relay the fill path's single doorbell to every feed. `ring_all`'s
    // `try_send`: a full per-feed channel already means "reload pending", and
    // this must never block. Cloned rather than moved so a later credential
    // reload can still register a replacement doorbell through the same
    // `DoorbellRegistry` after this task has started — see its doc comment.
    let fanout_doorbells = state.doorbells().clone();
    tokio::spawn(async move {
        while position_changed_rx.recv().await.is_some() {
            fanout_doorbells.ring_all();
        }
    });

    // Populate an empty catalog from brokers in the background, so the minutes-long
    // option-chain fetch never delays the server binding below. `auto_sync_pending`
    // and `synced_brokers` were both computed above (catalog empty + which brokers
    // the store credentials).
    if auto_sync_pending {
        setup::bootstrap::spawn_sync(synced_brokers);
    }

    // Register routes

    // 1) Register order routes
    let orders_router = Router::new()
        .route("/orders/submit", post(handlers::orders_submit))
        .route("/orders/cancel", post(handlers::orders_cancel))
        .route("/orders/:id", get(handlers::get_order))
        .route("/orders", get(handlers::list_orders))
        .route("/portfolios", get(handlers::list_portfolios))
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
            "/admin/broker-connections/:code/credentials",
            get(admin::get_broker_connection_credentials)
                .put(admin::put_broker_connection_credentials)
                .delete(admin::delete_broker_connection_credentials),
        )
        .route(
            "/admin/broker-connections/:code/credentials/test",
            post(admin::test_broker_connection_credentials),
        )
        .route("/admin/connections/reload", post(admin::reload_connections))
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
        .route("/admin/feeds", get(admin::list_feeds))
        .route("/admin/symbology/resolve", post(admin::resolve_symbology))
        .route("/admin/symbology/backfill", post(admin::backfill_symbology))
        .route("/admin/instruments/expiry-sweep", post(admin::expiry_sweep))
        .route("/admin/setup-status", get(admin::setup_status))
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

    // Start TCP listener
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    let host_url = format!("http://{}", bind_addr);
    info!("OMS listening on {}", host_url);
    info!("Scalar UI: {}/scalar", host_url);
    info!("OpenAPI spec: {}/api-docs/openapi.json", host_url);
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::bind_is_loopback;
    use super::{admin_password_from_env, nothing_decrypted, resolve_admin_password, resolve_bind_addr, DEFAULT_BIND_ADDR};
    use crate::config::FileConfig;

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
        assert!(json.contains("SaveResponse"));
        assert!(json.contains("TestResponse"));
        assert!(json.contains("CredentialSubmission"));
    }

    /// Serialise env mutation: `admin_password_env_falls_through_to_token` shares
    /// process-global env state with every other test in the binary.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The defaults-are-fine-on-a-laptop case: these must accept the built-in
    /// admin password, with or without a port.
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
    fn admin_password_prefers_env_then_file() {
        let file = crate::config::parse("[server]\nadmin_password = \"from-file\"\n").expect("parse");

        assert_eq!(resolve_admin_password(Some("from-env".into()), Some(&file)).as_deref(), Some("from-env"));
        assert_eq!(resolve_admin_password(None, Some(&file)).as_deref(), Some("from-file"));
        assert_eq!(resolve_admin_password(None, Some(&FileConfig::default())), None);
        assert_eq!(resolve_admin_password(None, None), None);
    }

    /// A blank `admin_password` in the file must not satisfy the off-loopback
    /// refusal — it is treated the same as absent, not as a configured value.
    #[test]
    fn admin_password_blank_in_file_is_unconfigured() {
        let file = crate::config::parse("[server]\nadmin_password = \"\"\n").expect("parse");
        assert_eq!(resolve_admin_password(None, Some(&file)), None);
    }

    /// Regression: `OMS_ADMIN_PASSWORD=""` must fall through to `OMS_ADMIN_TOKEN`,
    /// not swallow it. `Option::or_else` only fires on `None`, so the emptiness
    /// filter on `OMS_ADMIN_PASSWORD` has to run before the `or_else`.
    #[test]
    fn admin_password_env_falls_through_to_token_when_password_is_empty() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("OMS_ADMIN_PASSWORD", "");
        std::env::set_var("OMS_ADMIN_TOKEN", "from-token");

        let result = admin_password_from_env();

        std::env::remove_var("OMS_ADMIN_PASSWORD");
        std::env::remove_var("OMS_ADMIN_TOKEN");

        assert_eq!(result.as_deref(), Some("from-token"));
    }
}
