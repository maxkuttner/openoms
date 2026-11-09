//! Durable co-located deployment registration, bootstrap snapshots and
//! kill/enable command identity. The database, not a cached registry, owns the
//! account assignment, epoch, policy revision and kill latch.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use tracing::info;
use uuid::Uuid;

use colo_link::v2::{Allocation, Bootstrap, Policy, RuntimeCheckpoint, Timings, ToRuntime};
use gateway::runtime::{AccountIdentity, ExecutionCursor as Cursor, InstrumentRules, Operation, SymbolCursor, VenueCapabilities};

use crate::admin::AdminError;
use crate::app_state::AppState;

const DEPLOYMENT_SELECT: &str = "id, strategy_code, account_id, venue, environment, \
    external_account_ref, epoch, status, authority_secs, market_data_max_age_secs, heartbeat_secs, \
    allocation_quote_asset, allocation_max_notional::double precision AS allocation_max_notional, \
    allocation_max_position_qty::double precision AS allocation_max_position_qty, \
    allocation_fee_allowance_bps::double precision AS allocation_fee_allowance_bps, policy_revision, \
    kill_epoch, kill_command_id, verified_stop_at, verified_stop_epoch, retired_at, \
    shutdown_evidence, created_at, updated_at, \
    (SELECT c.ack_stage FROM colo_deployment_command c WHERE c.command_id = kill_command_id) AS kill_ack_stage, \
    acked_policy_revision, recovery_ready, recovery_missing, recovery_reported_at, \
    inventory_shortfall, inventory_reported_at";

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Deployment {
    pub id: Uuid,
    pub strategy_code: String,
    pub account_id: Uuid,
    pub venue: String,
    pub environment: String,
    pub external_account_ref: String,
    pub epoch: i64,
    pub status: String,
    pub authority_secs: i32,
    pub market_data_max_age_secs: i32,
    pub heartbeat_secs: i32,
    pub allocation_quote_asset: String,
    pub allocation_max_notional: f64,
    pub allocation_max_position_qty: Option<f64>,
    pub allocation_fee_allowance_bps: f64,
    pub policy_revision: i64,
    pub kill_epoch: Option<i64>,
    pub kill_command_id: Option<String>,
    pub verified_stop_at: Option<DateTime<Utc>>,
    pub verified_stop_epoch: Option<i64>,
    pub retired_at: Option<DateTime<Utc>>,
    pub shutdown_evidence: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How far the runtime has acknowledged the active kill:
    /// RECEIVED, ADMISSION_STOPPED, CANCELS_SENT or CONFIRMED_CLEAR.
    pub kill_ack_stage: Option<String>,
    /// Newest policy revision the runtime reports as installed.
    pub acked_policy_revision: Option<i64>,
    /// The runtime's last recovery report: ready to trade, and what was
    /// missing if not (one item per line).
    pub recovery_ready: Option<bool>,
    pub recovery_missing: Option<String>,
    pub recovery_reported_at: Option<DateTime<Utc>>,
    /// Venue balances the runtime found short of its expected inventory, one
    /// asset per line; trading is stopped until a rebaseline. NULL when clean.
    pub inventory_shortfall: Option<String>,
    pub inventory_reported_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct InstrumentBaseline {
    pub instrument_id: i64,
    pub quantity: f64,
    pub cost_basis: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AssetHolding {
    pub asset: String,
    pub quantity: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct DeploymentRegistration {
    #[serde(default)]
    pub strategy_code: String,
    pub account_id: Uuid,
    pub venue: String,
    pub environment: String,
    pub external_account_ref: String,
    pub authority_secs: i32,
    pub market_data_max_age_secs: i32,
    #[serde(default)]
    pub heartbeat_secs: Option<i32>,
    pub allocation_quote_asset: String,
    pub allocation_max_notional: f64,
    pub allocation_max_position_qty: Option<f64>,
    /// Fee headroom held back on every buy, in basis points of its notional
    /// (0 to 1000). Required: e.g. 10 for a 0.1% taker fee.
    pub allocation_fee_allowance_bps: f64,
    /// Explicit allowed universe, each with its opening position/cost baseline.
    pub instruments: Vec<InstrumentBaseline>,
    /// Explicit per-asset opening holdings; never inferred from instrument pairs.
    pub assets: Vec<AssetHolding>,
}

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct RetireRequest {
    pub shutdown_evidence: String,
    pub verified_shutdown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityGrant {
    pub deployment_id: Uuid,
    pub epoch: u64,
    pub session_nonce: Uuid,
    pub authority_secs: u32,
    pub policy_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandKind {
    Kill,
    Enable { stopped_epoch: u64 },
}

#[derive(Debug, Clone)]
pub struct ControlCommand {
    pub deployment_id: Uuid,
    pub epoch: u64,
    pub command_id: String,
    pub kind: CommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    pub deployment_id: Uuid,
    pub epoch: u64,
    pub command_id: String,
    pub kind: &'static str,
    pub stopped_epoch: Option<u64>,
}

impl CommandReceipt {
    /// ACK handling is exact: the same command id and epoch must be echoed.
    pub fn acknowledges(&self, command: &ControlCommand) -> bool {
        self.deployment_id == command.deployment_id
            && self.command_id == command.command_id
            && self.epoch == command.epoch
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentError {
    NotFound,
    Conflict(String),
    AliasInUse,
    NotColocated,
    AccountMismatch(String),
    EnvironmentMismatch(String),
    InvalidInput(String),
    MissingMetadata(String),
    MissingBaseline(String),
    BaselineMismatch(String),
    BaselineSplit(String),
    InvalidPolicy(String),
    StaleEpoch { current: u64, got: u64 },
    EnableEpochMismatch { stopped: u64, active: Option<u64> },
    OwnershipNotVerified,
    Retired,
    Database(String),
}

impl std::fmt::Display for DeploymentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "deployment not found"),
            Self::Conflict(msg) => write!(f, "{msg}"),
            Self::AliasInUse => write!(f, "that venue account is already registered"),
            Self::NotColocated => write!(f, "strategy is not co-located"),
            Self::AccountMismatch(msg) => write!(f, "{msg}"),
            Self::EnvironmentMismatch(msg) => write!(f, "{msg}"),
            Self::InvalidInput(msg) => write!(f, "invalid deployment: {msg}"),
            Self::MissingMetadata(msg) => write!(f, "missing instrument metadata: {msg}"),
            Self::MissingBaseline(msg) => write!(f, "missing opening baseline: {msg}"),
            Self::BaselineMismatch(msg) => write!(f, "baseline conflicts with OMS positions: {msg}"),
            Self::BaselineSplit(msg) => write!(f, "ambiguous balance attribution: {msg}"),
            Self::InvalidPolicy(msg) => write!(f, "invalid policy: {msg}"),
            Self::StaleEpoch { current, got } => write!(f, "stale epoch {got}; current is {current}"),
            Self::EnableEpochMismatch { stopped, active } => {
                write!(f, "enable references epoch {stopped}, active kill is {active:?}")
            }
            Self::OwnershipNotVerified => {
                write!(f, "the former owner has not been verified stopped")
            }
            Self::Retired => write!(f, "deployment is retired"),
            Self::Database(msg) => write!(f, "database error: {msg}"),
        }
    }
}

impl std::error::Error for DeploymentError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError {
    NotFound,
    Retired,
    Incomplete(String),
    InvalidPolicy(String),
    Database(String),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "deployment not found"),
            Self::Retired => write!(f, "deployment is retired"),
            Self::Incomplete(msg) => write!(f, "bootstrap is incomplete: {msg}"),
            Self::InvalidPolicy(msg) => write!(f, "invalid policy: {msg}"),
            Self::Database(msg) => write!(f, "database error: {msg}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PolicyPayload {
    limits: BTreeMap<String, risk::Limits>,
    trading_state: risk::TradingState,
    instrument_states: BTreeMap<String, risk::TradingState>,
}

fn db(e: sqlx::Error) -> DeploymentError {
    if let sqlx::Error::Database(err) = &e {
        if err.code().as_deref() == Some("23505") {
            return DeploymentError::AliasInUse;
        }
    }
    DeploymentError::Database(e.to_string())
}

fn db_bootstrap(e: sqlx::Error) -> BootstrapError {
    BootstrapError::Database(e.to_string())
}

fn validate_registration(input: &DeploymentRegistration) -> Result<(), DeploymentError> {
    if input.strategy_code.trim().is_empty() {
        return Err(DeploymentError::InvalidInput("strategy code is required".into()));
    }
    if input.venue.trim().is_empty() || input.external_account_ref.trim().is_empty() {
        return Err(DeploymentError::InvalidInput(
            "venue and external account reference are required".into(),
        ));
    }
    if input.environment != "PAPER" && input.environment != "LIVE" {
        return Err(DeploymentError::InvalidInput("environment must be PAPER or LIVE".into()));
    }
    if input.authority_secs <= 0 || input.market_data_max_age_secs <= 0 {
        return Err(DeploymentError::InvalidInput(
            "authority and market-data age must be explicit and positive".into(),
        ));
    }
    if input.heartbeat_secs.is_some_and(|s| s <= 0) {
        return Err(DeploymentError::InvalidInput("heartbeat must be positive".into()));
    }
    if input.allocation_quote_asset.trim().is_empty()
        || !input.allocation_max_notional.is_finite()
        || input.allocation_max_notional <= 0.0
    {
        return Err(DeploymentError::InvalidInput("allocation must be positive".into()));
    }
    if input
        .allocation_max_position_qty
        .is_some_and(|q| !q.is_finite() || q <= 0.0)
    {
        return Err(DeploymentError::InvalidInput("allocation position must be positive".into()));
    }
    if !input.allocation_fee_allowance_bps.is_finite()
        || !(0.0..=colo_link::v2::MAX_FEE_ALLOWANCE_BPS).contains(&input.allocation_fee_allowance_bps)
    {
        return Err(DeploymentError::InvalidInput("fee allowance must be 0 to 1000 bps".into()));
    }
    if input.instruments.is_empty() {
        return Err(DeploymentError::MissingBaseline("allowed instrument universe is empty".into()));
    }
    if input.assets.is_empty() {
        return Err(DeploymentError::MissingBaseline("per-asset opening holdings are required".into()));
    }
    let mut ids = BTreeSet::new();
    for item in &input.instruments {
        if !ids.insert(item.instrument_id) {
            return Err(DeploymentError::InvalidInput("instrument listed twice".into()));
        }
        if !item.quantity.is_finite() || !item.cost_basis.is_finite() {
            return Err(DeploymentError::InvalidInput("baseline values must be finite".into()));
        }
    }
    let mut assets = BTreeSet::new();
    for asset in &input.assets {
        if asset.asset.trim().is_empty() || !asset.quantity.is_finite() || asset.quantity < 0.0 {
            return Err(DeploymentError::InvalidInput(
                "asset holdings must be finite and non-negative".into(),
            ));
        }
        if !assets.insert(asset.asset.as_str()) {
            return Err(DeploymentError::InvalidInput("asset listed twice".into()));
        }
    }
    Ok(())
}

pub async fn register_deployment(
    pool: &PgPool,
    input: &DeploymentRegistration,
) -> Result<Deployment, DeploymentError> {
    validate_registration(input)?;
    let mut tx = pool.begin().await.map_err(db)?;

    let strategy = sqlx::query(
        "SELECT portfolio_id, mode, follow_environment FROM strategy WHERE code = $1 FOR UPDATE",
    )
    .bind(&input.strategy_code)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(DeploymentError::NotFound)?;
    if strategy.get::<String, _>("mode") != "COLOCATED" {
        return Err(DeploymentError::NotColocated);
    }
    let portfolio_id: Uuid = strategy.get("portfolio_id");
    if let Some(follow) = strategy.get::<Option<String>, _>("follow_environment") {
        if follow != input.environment {
            return Err(DeploymentError::EnvironmentMismatch(format!(
                "strategy follows {follow}, deployment is {}",
                input.environment
            )));
        }
    }

    let account = sqlx::query(
        "SELECT a.portfolio_id, a.external_account_ref, bc.environment, bc.broker_code \
         FROM account a JOIN broker_connection bc ON bc.code = a.broker_connection_code \
         WHERE a.id = $1",
    )
    .bind(input.account_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(|| DeploymentError::AccountMismatch("account not found".into()))?;
    if account.get::<Option<Uuid>, _>("portfolio_id") != Some(portfolio_id) {
        return Err(DeploymentError::AccountMismatch(
            "account does not belong to the strategy portfolio".into(),
        ));
    }
    if account.get::<String, _>("broker_code") != input.venue {
        return Err(DeploymentError::AccountMismatch("account is on another broker".into()));
    }
    if account.get::<String, _>("environment") != input.environment {
        return Err(DeploymentError::EnvironmentMismatch(format!(
            "account is {}, deployment is {}",
            account.get::<String, _>("environment"),
            input.environment
        )));
    }
    if account.get::<String, _>("external_account_ref") != input.external_account_ref {
        return Err(DeploymentError::AccountMismatch(
            "external account reference does not match the verified account".into(),
        ));
    }

    let deployment_id = Uuid::new_v4();
    let heartbeat = input.heartbeat_secs.unwrap_or(1);
    let deployment = sqlx::query_as::<_, Deployment>(&format!(
        "INSERT INTO colo_deployment \
         (id, strategy_code, account_id, venue, environment, external_account_ref, authority_secs, \
          market_data_max_age_secs, heartbeat_secs, allocation_quote_asset, allocation_max_notional, \
          allocation_max_position_qty, allocation_fee_allowance_bps) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING {DEPLOYMENT_SELECT}"
    ))
    .bind(deployment_id)
    .bind(&input.strategy_code)
    .bind(input.account_id)
    .bind(&input.venue)
    .bind(&input.environment)
    .bind(&input.external_account_ref)
    .bind(input.authority_secs)
    .bind(input.market_data_max_age_secs)
    .bind(heartbeat)
    .bind(&input.allocation_quote_asset)
    .bind(input.allocation_max_notional)
    .bind(input.allocation_max_position_qty)
    .bind(input.allocation_fee_allowance_bps)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;

    for item in &input.instruments {
        let metadata = instrument_metadata(&mut tx, &input.venue, item.instrument_id).await?;
        let existing: Option<f64> = sqlx::query_scalar(
            "SELECT net_qty::double precision FROM position \
             WHERE portfolio_id = $1 AND instrument_id = $2::text",
        )
        .bind(portfolio_id)
        .bind(item.instrument_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if let Some(existing) = existing {
            if (existing - item.quantity).abs() > 1e-9 {
                return Err(DeploymentError::BaselineMismatch(format!(
                    "{} existing OMS position {existing} != baseline {}",
                    metadata.symbol, item.quantity
                )));
            }
        }
        sqlx::query(
            "INSERT INTO colo_deployment_universe (deployment_id, instrument_id) VALUES ($1, $2)",
        )
        .bind(deployment_id)
        .bind(item.instrument_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO colo_deployment_baseline (deployment_id, instrument_id, quantity, cost_basis) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(deployment_id)
        .bind(item.instrument_id)
        .bind(item.quantity)
        .bind(item.cost_basis)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    for asset in &input.assets {
        sqlx::query(
            "INSERT INTO colo_deployment_asset_baseline (deployment_id, asset, quantity) VALUES ($1, $2, $3)",
        )
        .bind(deployment_id)
        .bind(&asset.asset)
        .bind(asset.quantity)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "INSERT INTO colo_asset_balance (account_id, asset, quantity) VALUES ($1, $2, $3) \
             ON CONFLICT (account_id, asset) DO UPDATE SET quantity = EXCLUDED.quantity, updated_at = now()",
        )
        .bind(input.account_id)
        .bind(&asset.asset)
        .bind(asset.quantity)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }

    let policy = policy_for(&mut tx, deployment_id, &input.strategy_code, portfolio_id).await?;
    sqlx::query(
        "INSERT INTO colo_deployment_policy (deployment_id, revision, payload) VALUES ($1, 1, $2)",
    )
    .bind(deployment_id)
    .bind(serde_json::to_value(&policy).map_err(|e| DeploymentError::InvalidPolicy(e.to_string()))?)
    .execute(&mut *tx)
    .await
    .map_err(db)?;

    tx.commit().await.map_err(db)?;
    info!(deployment = %deployment_id, strategy = %input.strategy_code, "co-located deployment registered");
    Ok(deployment)
}

struct Metadata {
    symbol: String,
    currency: String,
    price_increment: f64,
    size_increment: f64,
    contract_size: f64,
    broker_symbol: String,
    native_id: Option<String>,
    min_quantity: Option<f64>,
    max_quantity: Option<f64>,
    min_notional: Option<f64>,
    max_notional: Option<f64>,
}

async fn instrument_metadata(
    conn: &mut PgConnection,
    venue: &str,
    instrument_id: i64,
) -> Result<Metadata, DeploymentError> {
    let row = sqlx::query(
        "SELECT i.symbol, i.currency, i.price_increment::double precision AS price_increment, \
                i.size_increment::double precision AS size_increment, \
                i.contract_size::double precision AS contract_size, \
                bi.broker_symbol, bi.native_id, bi.min_quantity::double precision AS min_quantity, \
                bi.max_quantity::double precision AS max_quantity, \
                bi.min_notional::double precision AS min_notional, \
                bi.max_notional::double precision AS max_notional \
         FROM instrument i \
         LEFT JOIN broker_instrument bi ON bi.instrument_id = i.id AND bi.broker_code = $2 \
         WHERE i.id = $1",
    )
    .bind(instrument_id)
    .bind(venue)
    .fetch_optional(conn)
    .await
    .map_err(db)?
    .ok_or_else(|| DeploymentError::MissingMetadata(format!("instrument {instrument_id} absent")))?;
    let broker_symbol: Option<String> = row.get("broker_symbol");
    let metadata = Metadata {
        symbol: row.get("symbol"),
        currency: row.get("currency"),
        price_increment: row.get("price_increment"),
        size_increment: row.get("size_increment"),
        contract_size: row.get("contract_size"),
        broker_symbol: broker_symbol.clone().ok_or_else(|| {
            DeploymentError::MissingMetadata(format!("instrument {instrument_id} has no {venue} mapping"))
        })?,
        native_id: row.get("native_id"),
        min_quantity: row.get("min_quantity"),
        max_quantity: row.get("max_quantity"),
        min_notional: row.get("min_notional"),
        max_notional: row.get("max_notional"),
    };
    if metadata.price_increment <= 0.0
        || metadata.size_increment <= 0.0
        || metadata.contract_size <= 0.0
        || !metadata.price_increment.is_finite()
        || !metadata.size_increment.is_finite()
        || !metadata.contract_size.is_finite()
    {
        return Err(DeploymentError::MissingMetadata(format!(
            "instrument {instrument_id} has no usable tick/step/multiplier"
        )));
    }
    Ok(metadata)
}

async fn policy_for(
    conn: &mut PgConnection,
    deployment_id: Uuid,
    strategy_code: &str,
    portfolio_id: Uuid,
) -> Result<PolicyPayload, DeploymentError> {
    let strategy = sqlx::query(
        "SELECT state, max_order_quantity::double precision AS max_order_quantity, \
                max_order_notional::double precision AS max_order_notional, \
                max_position_quantity::double precision AS max_position_quantity, \
                max_position_notional::double precision AS max_position_notional, \
                price_band_pct::double precision AS price_band_pct, max_orders_per_sec \
         FROM strategy WHERE code = $1",
    )
    .bind(strategy_code)
    .fetch_one(&mut *conn)
    .await
    .map_err(db)?;
    let strategy_state = parse_state(strategy.get::<String, _>("state").as_str())
        .ok_or_else(|| DeploymentError::InvalidPolicy("unknown strategy state".into()))?;
    let defaults = risk::Limits {
        max_order_qty: strategy.get("max_order_quantity"),
        max_order_notional: strategy.get("max_order_notional"),
        max_position_qty: strategy.get("max_position_quantity"),
        max_position_notional: strategy.get("max_position_notional"),
        price_band_pct: strategy.get("price_band_pct"),
        max_orders_per_sec: strategy
            .get::<Option<i32>, _>("max_orders_per_sec")
            .map(|v| v.max(1) as u32),
    };

    let universe: Vec<i64> = sqlx::query_scalar(
        "SELECT instrument_id FROM colo_deployment_universe WHERE deployment_id = $1",
    )
    .bind(deployment_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;

    let mut limits = BTreeMap::new();
    let mut instrument_states = BTreeMap::new();
    for instrument_id in universe {
        let instrument = sqlx::query("SELECT symbol, currency FROM instrument WHERE id = $1")
            .bind(instrument_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?
            .ok_or_else(|| DeploymentError::MissingMetadata(format!("instrument {instrument_id} absent")))?;
        let symbol: String = instrument.get("symbol");
        let currency: String = instrument.get("currency");
        let overrides = sqlx::query(
            "SELECT trading_state, max_order_quantity::double precision AS max_order_quantity, \
                    max_order_notional::double precision AS max_order_notional, \
                    max_position_quantity::double precision AS max_position_quantity, \
                    max_position_notional::double precision AS max_position_notional \
             FROM risk_limits WHERE portfolio_id = $1 AND instrument_id = $2::text",
        )
        .bind(portfolio_id)
        .bind(instrument_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?;
        let override_limits = overrides.as_ref().map_or_else(risk::Limits::default, |row| risk::Limits {
            max_order_qty: row.get("max_order_quantity"),
            max_order_notional: row.get("max_order_notional"),
            max_position_qty: row.get("max_position_quantity"),
            max_position_notional: row.get("max_position_notional"),
            ..Default::default()
        });
        let resolved = risk::resolve_limits(&defaults, &override_limits, &currency)
            .map_err(|e| DeploymentError::InvalidPolicy(e.to_string()))?;
        let state = overrides
            .as_ref()
            .and_then(|row| parse_state(row.get::<String, _>("trading_state").as_str()))
            .unwrap_or(strategy_state);
        limits.insert(symbol.clone(), resolved);
        instrument_states.insert(symbol, state);
    }
    Ok(PolicyPayload { limits, trading_state: strategy_state, instrument_states })
}

fn parse_state(raw: &str) -> Option<risk::TradingState> {
    match raw {
        "ACTIVE" => Some(risk::TradingState::Active),
        "REDUCING" => Some(risk::TradingState::Reducing),
        "HALTED" => Some(risk::TradingState::Halted),
        _ => None,
    }
}

pub async fn list_deployments(pool: &PgPool, strategy_code: &str) -> Result<Vec<Deployment>, DeploymentError> {
    sqlx::query_as::<_, Deployment>(&format!(
        "SELECT {DEPLOYMENT_SELECT} FROM colo_deployment WHERE strategy_code = $1 ORDER BY created_at DESC"
    ))
    .bind(strategy_code)
    .fetch_all(pool)
    .await
    .map_err(db)
}

pub async fn get_deployment(pool: &PgPool, strategy_code: &str, id: Uuid) -> Result<Deployment, DeploymentError> {
    sqlx::query_as::<_, Deployment>(&format!(
        "SELECT {DEPLOYMENT_SELECT} FROM colo_deployment WHERE id = $1 AND strategy_code = $2"
    ))
    .bind(id)
    .bind(strategy_code)
    .fetch_optional(pool)
    .await
    .map_err(db)?
    .ok_or(DeploymentError::NotFound)
}

pub async fn acquire_session(
    pool: &PgPool,
    deployment_id: Uuid,
    session_nonce: Uuid,
) -> Result<AuthorityGrant, DeploymentError> {
    let mut tx = pool.begin().await.map_err(db)?;
    let deployment = sqlx::query(
        "SELECT epoch, status, authority_secs, policy_revision, verified_stop_epoch \
         FROM colo_deployment WHERE id = $1 FOR UPDATE",
    )
    .bind(deployment_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(DeploymentError::NotFound)?;
    if deployment.get::<String, _>("status") != "ACTIVE" {
        return Err(DeploymentError::Retired);
    }
    let mut epoch = deployment.get::<i64, _>("epoch") as u64;
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT session_nonce FROM colo_deployment_session WHERE deployment_id = $1 FOR UPDATE",
    )
    .bind(deployment_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if let Some(existing) = existing {
        if existing != session_nonce {
            let verified = deployment
                .get::<Option<i64>, _>("verified_stop_epoch")
                .is_some_and(|verified| verified as u64 == epoch);
            if !verified {
                return Err(DeploymentError::OwnershipNotVerified);
            }
            epoch = epoch.saturating_add(1);
            sqlx::query("UPDATE colo_deployment SET epoch = $2, verified_stop_at = NULL, verified_stop_epoch = NULL, updated_at = now() WHERE id = $1")
                .bind(deployment_id)
                .bind(epoch as i64)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
    }
    sqlx::query(
        "INSERT INTO colo_deployment_session (deployment_id, session_nonce) VALUES ($1, $2) \
         ON CONFLICT (deployment_id) DO UPDATE SET session_nonce = EXCLUDED.session_nonce, acquired_at = now()",
    )
    .bind(deployment_id)
    .bind(session_nonce)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(AuthorityGrant {
        deployment_id,
        epoch,
        session_nonce,
        authority_secs: deployment.get::<i32, _>("authority_secs").max(1) as u32,
        policy_revision: deployment.get::<i64, _>("policy_revision") as u64,
    })
}

pub async fn apply_command(
    pool: &PgPool,
    command: &ControlCommand,
) -> Result<CommandReceipt, DeploymentError> {
    if command.command_id.trim().is_empty() {
        return Err(DeploymentError::InvalidInput("command id is required".into()));
    }
    let mut tx = pool.begin().await.map_err(db)?;
    let deployment = sqlx::query(
        "SELECT epoch, status, kill_epoch, kill_command_id FROM colo_deployment WHERE id = $1 FOR UPDATE",
    )
    .bind(command.deployment_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(DeploymentError::NotFound)?;
    if deployment.get::<String, _>("status") != "ACTIVE" {
        return Err(DeploymentError::Retired);
    }
    let current = deployment.get::<i64, _>("epoch") as u64;
    if command.epoch != current {
        return Err(DeploymentError::StaleEpoch { current, got: command.epoch });
    }

    if let Some(existing) = sqlx::query(
        "SELECT deployment_id, command_id, kind, epoch, prior_epoch FROM colo_deployment_command \
         WHERE command_id = $1",
    )
    .bind(&command.command_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    {
        let same = existing.get::<Uuid, _>("deployment_id") == command.deployment_id
            && existing.get::<i64, _>("epoch") as u64 == command.epoch;
        if !same {
            return Err(DeploymentError::Conflict("command id already used".into()));
        }
        let kind = existing.get::<String, _>("kind");
        let prior = existing.get::<Option<i64>, _>("prior_epoch").map(|v| v as u64);
        return Ok(CommandReceipt {
            deployment_id: command.deployment_id,
            epoch: command.epoch,
            command_id: command.command_id.clone(),
            kind: if kind == "KILL" { "kill" } else { "enable" },
            stopped_epoch: if kind == "ENABLE" { prior } else { None },
        });
    }

    let (kind, prior) = match command.kind {
        CommandKind::Kill => ("KILL", deployment.get::<Option<i64>, _>("kill_epoch").map(|v| v as u64)),
        CommandKind::Enable { stopped_epoch } => {
            let active = deployment.get::<Option<i64>, _>("kill_epoch").map(|v| v as u64);
            if active != Some(stopped_epoch) {
                return Err(DeploymentError::EnableEpochMismatch { stopped: stopped_epoch, active });
            }
            ("ENABLE", Some(stopped_epoch))
        }
    };
    sqlx::query(
        "INSERT INTO colo_deployment_command (id, deployment_id, command_id, kind, epoch, prior_epoch) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(command.deployment_id)
    .bind(&command.command_id)
    .bind(kind)
    .bind(command.epoch as i64)
    .bind(prior.map(|v| v as i64))
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    match command.kind {
        CommandKind::Kill => {
            sqlx::query(
                "UPDATE colo_deployment SET kill_epoch = $2, kill_command_id = $3, updated_at = now() WHERE id = $1",
            )
            .bind(command.deployment_id)
            .bind(command.epoch as i64)
            .bind(&command.command_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        CommandKind::Enable { .. } => {
            sqlx::query(
                "UPDATE colo_deployment SET kill_epoch = NULL, kill_command_id = NULL, updated_at = now() WHERE id = $1",
            )
            .bind(command.deployment_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(CommandReceipt {
        deployment_id: command.deployment_id,
        epoch: command.epoch,
        command_id: command.command_id.clone(),
        kind: if kind == "KILL" { "kill" } else { "enable" },
        stopped_epoch: if kind == "ENABLE" { prior } else { None },
    })
}

/// The runtime should report a kill's admission stopped this soon, as the
/// v1 link must acknowledge one.
pub const KILL_STOPPED_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);
/// And confirm nothing rests at the venue this soon. Longer: cancels go
/// through the venue, which the spec says cannot be guaranteed within 5 s.
pub const KILL_CLEAR_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether a kill sent `age` ago is overdue at its acknowledged `stage`.
pub fn kill_stalled(stage: Option<&str>, age: std::time::Duration) -> bool {
    let stopped = matches!(stage, Some("ADMISSION_STOPPED" | "CANCELS_SENT" | "CONFIRMED_CLEAR"));
    let clear = stage == Some("CONFIRMED_CLEAR");
    (!stopped && age > KILL_STOPPED_WITHIN) || (!clear && age > KILL_CLEAR_WITHIN)
}

/// Strategies whose active deployment holds a kill the runtime has not
/// acknowledged in time (see `kill_stalled`).
pub async fn stalled_kills(pool: &PgPool) -> Result<std::collections::HashSet<String>, DeploymentError> {
    let rows = sqlx::query(
        "SELECT d.strategy_code, c.ack_stage, \
                EXTRACT(EPOCH FROM (now() - c.created_at))::double precision AS age_secs \
         FROM colo_deployment d JOIN colo_deployment_command c ON c.command_id = d.kill_command_id \
         WHERE d.status = 'ACTIVE' AND d.kill_epoch IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            let age = std::time::Duration::from_secs_f64(row.get::<f64, _>("age_secs").max(0.0));
            kill_stalled(row.get::<Option<String>, _>("ack_stage").as_deref(), age)
        })
        .map(|row| row.get::<String, _>("strategy_code"))
        .collect())
}

/// Records a kill/enable acknowledgement stage. Matches the command exactly
/// (deployment, command id and epoch) and only moves the stage forward, so a
/// late or replayed acknowledgement cannot undo a later one. Returns whether
/// anything changed.
pub async fn record_control_ack(
    pool: &PgPool,
    deployment_id: Uuid,
    epoch: u64,
    command_id: &str,
    stage: colo_link::v2::ControlAckStage,
) -> Result<bool, DeploymentError> {
    use colo_link::v2::ControlAckStage::*;
    let stage = match stage {
        Received => "RECEIVED",
        AdmissionStopped => "ADMISSION_STOPPED",
        CancelsSent => "CANCELS_SENT",
        ConfirmedClear => "CONFIRMED_CLEAR",
    };
    let updated = sqlx::query(
        "UPDATE colo_deployment_command SET ack_stage = $4, acked_at = now() \
         WHERE deployment_id = $1 AND command_id = $2 AND epoch = $3 \
           AND (ack_stage IS NULL OR \
                array_position(ARRAY['RECEIVED','ADMISSION_STOPPED','CANCELS_SENT','CONFIRMED_CLEAR'], ack_stage) \
              < array_position(ARRAY['RECEIVED','ADMISSION_STOPPED','CANCELS_SENT','CONFIRMED_CLEAR'], $4::text))",
    )
    .bind(deployment_id)
    .bind(command_id)
    .bind(epoch as i64)
    .bind(stage)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(updated.rows_affected() == 1)
}

/// Records the newest policy revision the runtime installed. A revision the
/// OMS has not published yet is ignored, and the value never goes back.
pub async fn record_policy_ack(pool: &PgPool, deployment_id: Uuid, revision: u64) -> Result<(), DeploymentError> {
    sqlx::query(
        "UPDATE colo_deployment \
         SET acked_policy_revision = GREATEST(COALESCE(acked_policy_revision, 0), $2), updated_at = now() \
         WHERE id = $1 AND $2 <= policy_revision AND $2 > 0",
    )
    .bind(deployment_id)
    .bind(revision as i64)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(())
}

/// Stores the recovery status the runtime last reported.
pub async fn record_recovery_status(
    pool: &PgPool,
    deployment_id: Uuid,
    ready: bool,
    missing: &[String],
) -> Result<(), DeploymentError> {
    sqlx::query(
        "UPDATE colo_deployment SET recovery_ready = $2, recovery_missing = $3, \
         recovery_reported_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(deployment_id)
    .bind(ready)
    .bind((!missing.is_empty()).then(|| missing.join("\n")))
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(())
}

/// Stores the runtime's latest inventory report; an empty one clears it.
pub async fn record_inventory_status(
    pool: &PgPool,
    deployment_id: Uuid,
    shortfalls: &[String],
) -> Result<(), DeploymentError> {
    sqlx::query(
        "UPDATE colo_deployment SET inventory_shortfall = $2, inventory_reported_at = now(), \
         updated_at = now() WHERE id = $1",
    )
    .bind(deployment_id)
    .bind((!shortfalls.is_empty()).then(|| shortfalls.join("\n")))
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(())
}

/// Records an operator rebaseline for the strategy's active deployment: the
/// runtime is to take the venue's current balances as its expected
/// inventory. Returns the command id to send, or `None` without a deployment.
pub async fn record_rebaseline_for_strategy(
    pool: &PgPool,
    strategy_code: &str,
) -> Result<Option<CommandReceipt>, DeploymentError> {
    let row = sqlx::query(
        "SELECT id, epoch FROM colo_deployment WHERE strategy_code = $1 AND status = 'ACTIVE' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(strategy_code)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let deployment_id: Uuid = row.get("id");
    let epoch = row.get::<i64, _>("epoch") as u64;
    let command_id = format!("rebaseline-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO colo_deployment_command (id, deployment_id, command_id, kind, epoch) \
         VALUES ($1, $2, $3, 'REBASELINE', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(deployment_id)
    .bind(&command_id)
    .bind(epoch as i64)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(Some(CommandReceipt { deployment_id, epoch, command_id, kind: "rebaseline", stopped_epoch: None }))
}

pub async fn record_kill_for_strategy(
    pool: &PgPool,
    strategy_code: &str,
) -> Result<Option<CommandReceipt>, DeploymentError> {
    let row = sqlx::query(
        "SELECT id, epoch FROM colo_deployment WHERE strategy_code = $1 AND status = 'ACTIVE' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(strategy_code)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    apply_command(
        pool,
        &ControlCommand {
            deployment_id: row.get("id"),
            epoch: row.get::<i64, _>("epoch") as u64,
            command_id: format!("kill-{}", Uuid::new_v4()),
            kind: CommandKind::Kill,
        },
    )
    .await
    .map(Some)
}

pub async fn record_enable_for_strategy(
    pool: &PgPool,
    strategy_code: &str,
) -> Result<Option<CommandReceipt>, DeploymentError> {
    let row = sqlx::query(
        "SELECT id, epoch, kill_epoch FROM colo_deployment \
         WHERE strategy_code = $1 AND status = 'ACTIVE' AND kill_epoch IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(strategy_code)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let stopped = row.get::<i64, _>("kill_epoch") as u64;
    apply_command(
        pool,
        &ControlCommand {
            deployment_id: row.get("id"),
            epoch: row.get::<i64, _>("epoch") as u64,
            command_id: format!("enable-{}", Uuid::new_v4()),
            kind: CommandKind::Enable { stopped_epoch: stopped },
        },
    )
    .await
    .map(Some)
}

pub async fn retire_deployment(
    pool: &PgPool,
    strategy_code: &str,
    id: Uuid,
    request: &RetireRequest,
) -> Result<Deployment, DeploymentError> {
    if !request.verified_shutdown || request.shutdown_evidence.trim().is_empty() {
        return Err(DeploymentError::InvalidInput(
            "retirement requires verified shutdown/fencing evidence".into(),
        ));
    }
    let mut tx = pool.begin().await.map_err(db)?;
    let deployment = sqlx::query_as::<_, Deployment>(&format!(
        "UPDATE colo_deployment SET status = 'RETIRED', retired_at = now(), verified_stop_at = now(), \
            verified_stop_epoch = epoch, shutdown_evidence = $3, updated_at = now() \
         WHERE id = $1 AND strategy_code = $2 AND status = 'ACTIVE' RETURNING {DEPLOYMENT_SELECT}"
    ))
    .bind(id)
    .bind(strategy_code)
    .bind(&request.shutdown_evidence)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(DeploymentError::NotFound)?;
    sqlx::query("DELETE FROM colo_deployment_session WHERE deployment_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(deployment)
}


#[derive(Debug, Clone)]
pub struct V2Binding {
    pub deployment: Deployment,
    pub epoch: u64,
    pub session_nonce: Uuid,
    pub account: AccountIdentity,
    pub authority_secs: u32,
}

/// Authorize one of the two v2 sockets. Control acquires (or reuses) the
/// deployment session; events must match the session already bound by control.
pub async fn authorize_v2(
    pool: &PgPool,
    strategy_code: &str,
    deployment_id: Uuid,
    session_nonce: Uuid,
    control: bool,
) -> Result<V2Binding, DeploymentError> {
    let deployment = get_deployment(pool, strategy_code, deployment_id).await?;
    if deployment.status != "ACTIVE" {
        return Err(DeploymentError::Retired);
    }
    let epoch = if control {
        acquire_session(pool, deployment_id, session_nonce).await?.epoch
    } else {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT session_nonce FROM colo_deployment_session WHERE deployment_id = $1",
        )
        .bind(deployment_id)
        .fetch_optional(pool)
        .await
        .map_err(db)?;
        if existing != Some(session_nonce) {
            return Err(DeploymentError::OwnershipNotVerified);
        }
        deployment.epoch as u64
    };
    let account = AccountIdentity {
        account_id: deployment.account_id,
        venue: deployment.venue.clone(),
        environment: deployment.environment.clone(),
        external_account_ref: deployment.external_account_ref.clone(),
    };
    Ok(V2Binding {
        authority_secs: deployment.authority_secs.max(1) as u32,
        deployment,
        epoch,
        session_nonce,
        account,
    })
}


/// Recompute every active deployment's complete policy snapshot, publish a new
/// revision when it differs, and push it over any live v2 control socket.
pub async fn publish_policy(state: &AppState, strategy_code: &str) -> Result<usize, DeploymentError> {
    let portfolio_id: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
        .bind(strategy_code)
        .fetch_optional(state.pool())
        .await
        .map_err(db)?
        .ok_or(DeploymentError::NotFound)?;
    let deployments: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, policy_revision FROM colo_deployment WHERE strategy_code = $1 AND status = 'ACTIVE'",
    )
    .bind(strategy_code)
    .fetch_all(state.pool())
    .await
    .map_err(db)?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    let mut updates = Vec::new();
    for (deployment_id, current_revision) in deployments {
        let policy = policy_for(&mut tx, deployment_id, strategy_code, portfolio_id).await?;
        let value = serde_json::to_value(&policy)
            .map_err(|e| DeploymentError::InvalidPolicy(e.to_string()))?;
        let stored: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT payload FROM colo_deployment_policy WHERE deployment_id = $1 ORDER BY revision DESC LIMIT 1",
        )
        .bind(deployment_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
        if stored.as_ref() == Some(&value) {
            continue;
        }
        let revision = current_revision.max(1) + 1;
        sqlx::query(
            "INSERT INTO colo_deployment_policy (deployment_id, revision, payload) VALUES ($1, $2, $3)",
        )
        .bind(deployment_id)
        .bind(revision)
        .bind(&value)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE colo_deployment SET policy_revision = $2, updated_at = now() WHERE id = $1")
            .bind(deployment_id)
            .bind(revision)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        updates.push((
            deployment_id,
            Policy {
                revision: revision as u64,
                limits: policy.limits,
                trading_state: policy.trading_state,
                instrument_states: policy.instrument_states,
            },
        ));
    }
    tx.commit().await.map_err(db)?;
    for (deployment_id, policy) in &updates {
        state
            .colo()
            .push_v2_control(*deployment_id, ToRuntime::Policy { policy: policy.clone() });
    }
    Ok(updates.len())
}

pub async fn bootstrap(state: &AppState, deployment: &Deployment) -> Result<Bootstrap, BootstrapError> {
    let mut tx = state.pool().begin().await.map_err(db_bootstrap)?;
    let current = sqlx::query_as::<_, Deployment>(&format!(
        "SELECT {DEPLOYMENT_SELECT} FROM colo_deployment WHERE id = $1 FOR UPDATE"
    ))
    .bind(deployment.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_bootstrap)?
    .ok_or(BootstrapError::NotFound)?;
    if current.status != "ACTIVE" {
        return Err(BootstrapError::Retired);
    }

    let portfolio_id = current_account_portfolio(&mut tx, current.account_id).await?;
    let policy = policy_for(&mut tx, current.id, &current.strategy_code, portfolio_id)
        .await
        .map_err(|e| match e {
            DeploymentError::Database(msg) => BootstrapError::Database(msg),
            other => BootstrapError::InvalidPolicy(other.to_string()),
        })?;
    let stored: Option<(i64, serde_json::Value)> = sqlx::query_as(
        "SELECT revision, payload FROM colo_deployment_policy WHERE deployment_id = $1 \
         ORDER BY revision DESC LIMIT 1",
    )
    .bind(current.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_bootstrap)?;
    let current_value = serde_json::to_value(&policy)
        .map_err(|e| BootstrapError::InvalidPolicy(e.to_string()))?;
    let revision = match &stored {
        Some((revision, payload)) if payload == &current_value => *revision as u64,
        Some((revision, _)) => (*revision as u64).saturating_add(1),
        None => current.policy_revision as u64,
    };
    if stored.as_ref().map_or(true, |(_, payload)| payload != &current_value) {
        sqlx::query(
            "INSERT INTO colo_deployment_policy (deployment_id, revision, payload) VALUES ($1, $2, $3)",
        )
        .bind(current.id)
        .bind(revision as i64)
        .bind(&current_value)
        .execute(&mut *tx)
        .await
        .map_err(db_bootstrap)?;
        sqlx::query("UPDATE colo_deployment SET policy_revision = $2, updated_at = now() WHERE id = $1")
            .bind(current.id)
            .bind(revision as i64)
            .execute(&mut *tx)
            .await
            .map_err(db_bootstrap)?;
    }
    let instruments = sqlx::query(
        "SELECT u.instrument_id, i.symbol, i.venue, i.currency, \
                i.price_increment::double precision AS price_increment, \
                i.size_increment::double precision AS size_increment, \
                i.contract_size::double precision AS contract_size, \
                bi.broker_symbol, bi.native_id, bi.min_quantity::double precision AS min_quantity, \
                bi.max_quantity::double precision AS max_quantity, \
                bi.min_notional::double precision AS min_notional, \
                bi.max_notional::double precision AS max_notional \
         FROM colo_deployment_universe u \
         JOIN instrument i ON i.id = u.instrument_id \
         JOIN broker_instrument bi ON bi.instrument_id = i.id AND bi.broker_code = $2 \
         WHERE u.deployment_id = $1 ORDER BY i.symbol",
    )
    .bind(current.id)
    .bind(&current.venue)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_bootstrap)?;
    if instruments.is_empty() {
        return Err(BootstrapError::Incomplete("deployment has no instrument rules".into()));
    }
    let mut rules = Vec::new();
    let mut symbols = Vec::new();
    for row in &instruments {
        let symbol: String = row.get("symbol");
        symbols.push(symbol.clone());
        rules.push(InstrumentRules {
            symbol,
            venue: row.get("venue"),
            venue_symbol: row.get("broker_symbol"),
            base_asset: row
                .get::<Option<String>, _>("native_id")
                .unwrap_or_else(|| row.get("symbol")),
            quote_asset: row.get("currency"),
            tick_size: row.get("price_increment"),
            step_size: row.get("size_increment"),
            multiplier: row.get("contract_size"),
            min_quantity: row.get("min_quantity"),
            max_quantity: row.get("max_quantity"),
            min_notional: row.get("min_notional"),
            max_notional: row.get("max_notional"),
            operations: vec![Operation::Place, Operation::Cancel],
            capabilities: VenueCapabilities {
                cancel_on_disconnect: false,
                atomic_cancel_replace: false,
                open_orders_snapshot: true,
                order_lookup: true,
                trade_history: true,
                balance_snapshot: true,
            },
        });
    }

    let positions = sqlx::query(
        "SELECT i.symbol, b.quantity::double precision AS quantity \
         FROM colo_deployment_baseline b JOIN instrument i ON i.id = b.instrument_id \
         WHERE b.deployment_id = $1",
    )
    .bind(current.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_bootstrap)?
    .into_iter()
    .map(|row| (row.get::<String, _>("symbol"), row.get::<f64, _>("quantity")))
    .collect();

    let assets = sqlx::query(
        "SELECT asset, quantity::double precision AS quantity FROM colo_deployment_asset_baseline \
         WHERE deployment_id = $1 ORDER BY asset",
    )
    .bind(current.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_bootstrap)?
    .into_iter()
    .map(|row| (row.get::<String, _>("asset"), row.get::<f64, _>("quantity")))
    .collect();

    let open_client_ids: Vec<String> = sqlx::query_scalar(
        "SELECT client_order_id FROM order_state \
         WHERE account_id = $1 AND status IN ('submitted', 'routed', 'partially_filled', 'suspended')",
    )
    .bind(current.account_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_bootstrap)?;

    let mut execution_cursor = Cursor::default();
    for symbol in &symbols {
        execution_cursor.positions.insert(
            symbol.clone(),
            SymbolCursor { last_trade_id: None, start_time: Some(current.created_at) },
        );
    }
    let baseline = RuntimeCheckpoint {
        sequence: revision,
        positions,
        open_client_ids,
        execution_cursor,
        complete: true,
    };
    let limits = policy.limits;
    let trading_state = policy.trading_state;
    let instrument_states = policy.instrument_states;
    tx.commit().await.map_err(db_bootstrap)?;

    Ok(Bootstrap {
        instruments: rules,
        limits,
        trading_state,
        instrument_states,
        timings: Timings {
            heartbeat_secs: current.heartbeat_secs.max(1) as u64,
            authority_secs: current.authority_secs.max(1) as u64,
            market_data_max_age_secs: current.market_data_max_age_secs.max(1) as u64,
        },
        baseline,
        asset_baseline: assets,
        killed_epoch: current.kill_epoch.map(|epoch| epoch as u64),
        allocation: Allocation {
            quote_asset: current.allocation_quote_asset,
            max_notional: current.allocation_max_notional,
            max_position_qty: current.allocation_max_position_qty,
            fee_allowance_bps: current.allocation_fee_allowance_bps,
        },
        recovery_universe: symbols,
    })
}

async fn current_account_portfolio(conn: &mut PgConnection, account_id: Uuid) -> Result<Uuid, BootstrapError> {
    sqlx::query_scalar("SELECT portfolio_id FROM account WHERE id = $1")
        .bind(account_id)
        .fetch_optional(conn)
        .await
        .map_err(db_bootstrap)?
        .ok_or_else(|| BootstrapError::Incomplete("deployment account not found".into()))
}

impl From<DeploymentError> for AdminError {
    fn from(value: DeploymentError) -> Self {
        use axum::http::StatusCode;
        let status = match value {
            DeploymentError::NotFound => StatusCode::NOT_FOUND,
            DeploymentError::AliasInUse | DeploymentError::Conflict(_) => StatusCode::CONFLICT,
            DeploymentError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        AdminError::new(status, value.to_string())
    }
}

#[utoipa::path(
    post, path = "/admin/strategies/{code}/deployments", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    request_body = DeploymentRegistration,
    responses((status = 200, description = "Registered", body = Deployment), (status = 400, description = "Invalid deployment"), (status = 409, description = "Account already registered")),
    security(("bearer_token" = []))
)]
pub async fn register(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(mut input): Json<DeploymentRegistration>,
) -> Result<Json<Deployment>, AdminError> {
    input.strategy_code = code;
    info!(strategy = %input.strategy_code, "admin register co-located deployment");
    register_deployment(state.pool(), &input).await.map(Json).map_err(Into::into)
}

#[utoipa::path(
    get, path = "/admin/strategies/{code}/deployments", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 200, description = "Deployments", body = [Deployment])),
    security(("bearer_token" = []))
)]
pub async fn list(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<Vec<Deployment>>, AdminError> {
    list_deployments(state.pool(), &code).await.map(Json).map_err(Into::into)
}

#[utoipa::path(
    get, path = "/admin/strategies/{code}/deployments/{id}", tag = "admin",
    params(("code" = String, Path, description = "Strategy code"), ("id" = Uuid, Path, description = "Deployment id")),
    responses((status = 200, description = "Deployment", body = Deployment), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    Path((code, id)): Path<(String, Uuid)>,
) -> Result<Json<Deployment>, AdminError> {
    get_deployment(state.pool(), &code, id).await.map(Json).map_err(Into::into)
}

#[utoipa::path(
    post, path = "/admin/strategies/{code}/deployments/{id}/retire", tag = "admin",
    params(("code" = String, Path, description = "Strategy code"), ("id" = Uuid, Path, description = "Deployment id")),
    request_body = RetireRequest,
    responses((status = 200, description = "Retired", body = Deployment), (status = 400, description = "Shutdown not verified"), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn retire(
    State(state): State<AppState>,
    Path((code, id)): Path<(String, Uuid)>,
    Json(request): Json<RetireRequest>,
) -> Result<Json<Deployment>, AdminError> {
    retire_deployment(state.pool(), &code, id, &request).await.map(Json).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: &str, epoch: u64) -> ControlCommand {
        ControlCommand {
            deployment_id: Uuid::nil(),
            epoch,
            command_id: id.into(),
            kind: CommandKind::Kill,
        }
    }

    #[test]
    fn a_kill_must_stop_admission_fast_and_clear_within_half_a_minute() {
        use std::time::Duration;
        let s = Duration::from_secs;
        assert!(!kill_stalled(None, s(4)), "still within its first 5 s");
        assert!(kill_stalled(None, s(6)));
        assert!(kill_stalled(Some("RECEIVED"), s(6)), "received is not stopped");
        assert!(!kill_stalled(Some("ADMISSION_STOPPED"), s(20)));
        assert!(!kill_stalled(Some("CANCELS_SENT"), s(29)));
        assert!(kill_stalled(Some("CANCELS_SENT"), s(31)), "orders may still rest");
        assert!(!kill_stalled(Some("CONFIRMED_CLEAR"), s(3_600)));
    }

    #[test]
    fn acknowledgement_must_match_command_and_epoch_exactly() {
        let command = command("kill-1", 7);
        let receipt = CommandReceipt {
            deployment_id: Uuid::nil(),
            epoch: 7,
            command_id: "kill-1".into(),
            kind: "kill",
            stopped_epoch: None,
        };
        assert!(receipt.acknowledges(&command));
        assert!(!(CommandReceipt { epoch: 6, ..receipt.clone() }).acknowledges(&command));
        assert!(!(CommandReceipt { command_id: "kill-2".into(), ..receipt }).acknowledges(&command));
    }

    #[test]
    fn registration_requires_explicit_timings_and_inventory() {
        let mut input = sample_registration();
        input.authority_secs = 0;
        assert!(matches!(validate_registration(&input), Err(DeploymentError::InvalidInput(_))));
        let mut input = sample_registration();
        input.assets.clear();
        assert!(matches!(validate_registration(&input), Err(DeploymentError::MissingBaseline(_))));
        let mut input = sample_registration();
        input.allocation_fee_allowance_bps = 1_500.0;
        assert!(matches!(validate_registration(&input), Err(DeploymentError::InvalidInput(_))));
        let mut input = sample_registration();
        input.instruments.clear();
        assert!(matches!(validate_registration(&input), Err(DeploymentError::MissingBaseline(_))));
    }

    fn sample_registration() -> DeploymentRegistration {
        DeploymentRegistration {
            strategy_code: "s".into(),
            account_id: Uuid::new_v4(),
            venue: "BINANCE".into(),
            environment: "PAPER".into(),
            external_account_ref: "acct".into(),
            authority_secs: 5,
            market_data_max_age_secs: 2,
            heartbeat_secs: Some(1),
            allocation_quote_asset: "USDT".into(),
            allocation_max_notional: 1000.0,
            allocation_max_position_qty: Some(10.0),
            allocation_fee_allowance_bps: 10.0,
            instruments: vec![InstrumentBaseline { instrument_id: 1, quantity: 0.0, cost_basis: 0.0 }],
            assets: vec![AssetHolding { asset: "USDT".into(), quantity: 1000.0 }],
        }
    }
    async fn isolated_pool() -> PgPool {
        let url = std::env::var("COLO_TEST_DATABASE_URL")
            .expect("set COLO_TEST_DATABASE_URL to a disposable database");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect isolated test database")
    }

    async fn seed_registration(
        pool: &PgPool,
        environment: &str,
    ) -> (String, Uuid, i64, DeploymentRegistration) {
        use crate::strategies::test_support as t;
        let code: String = t::seed_strategy(pool).await;
        let (symbol, instrument_id) = t::seed_pair(pool, &["BINANCE"]).await;
        let portfolio: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
            .bind(&code)
            .fetch_one(pool)
            .await
            .expect("portfolio");
        let suffix = Uuid::new_v4().simple().to_string();
        // Seeded here when missing: CI runs these against a fresh database.
        // One connection per broker and environment, shared by parallel tests.
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) \
             VALUES ('colo-test-binance-paper', 'BINANCE', 'PAPER', 'ACTIVE') \
             ON CONFLICT DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed BINANCE PAPER connection");
        let connection: String = sqlx::query_scalar(
            "SELECT code FROM broker_connection WHERE broker_code = 'BINANCE' AND environment = 'PAPER'",
        )
        .fetch_one(pool)
        .await
        .expect("BINANCE PAPER connection");
        let account = Uuid::new_v4();
        let external = format!("colo-{}", &suffix[..12]);
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
             VALUES ($1, $2, $3, $4, 'ACTIVE', $5)",
        )
        .bind(account)
        .bind(format!("colo-account-{suffix}"))
        .bind(&connection)
        .bind(&external)
        .bind(portfolio)
        .execute(pool)
        .await
        .expect("seed deployment account");
        sqlx::query("UPDATE strategy SET mode = 'COLOCATED', follow_environment = $2 WHERE code = $1")
            .bind(&code)
            .bind(environment)
            .execute(pool)
            .await
            .expect("colocated");
        let _ = symbol;
        let input = DeploymentRegistration {
            strategy_code: code.clone(),
            account_id: account,
            venue: "BINANCE".into(),
            environment: environment.into(),
            external_account_ref: external,
            authority_secs: 5,
            market_data_max_age_secs: 2,
            heartbeat_secs: Some(1),
            allocation_quote_asset: "USDT".into(),
            allocation_max_notional: 1_000.0,
            allocation_max_position_qty: Some(10.0),
            allocation_fee_allowance_bps: 10.0,
            instruments: vec![InstrumentBaseline { instrument_id, quantity: 0.0, cost_basis: 0.0 }],
            assets: vec![AssetHolding { asset: "USDT".into(), quantity: 1_000.0 }],
        };
        (code, account, instrument_id, input)
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn registration_refuses_a_via_oms_strategy() {
        let pool = isolated_pool().await;
        let (code, _, _, input) = seed_registration(&pool, "PAPER").await;
        sqlx::query("UPDATE strategy SET mode = 'VIA_OMS' WHERE code = $1")
            .bind(&code)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            register_deployment(&pool, &input).await,
            Err(DeploymentError::NotColocated)
        ));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn registration_refuses_the_wrong_environment() {
        let pool = isolated_pool().await;
        let (code, _, _, mut input) = seed_registration(&pool, "PAPER").await;
        sqlx::query("UPDATE strategy SET follow_environment = NULL WHERE code = $1")
            .bind(&code)
            .execute(&pool)
            .await
            .unwrap();
        input.environment = "LIVE".into();
        assert!(matches!(
            register_deployment(&pool, &input).await,
            Err(DeploymentError::EnvironmentMismatch(_))
        ));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn registration_requires_metadata_and_a_baseline() {
        let pool = isolated_pool().await;
        let (_, _, _, mut input) = seed_registration(&pool, "PAPER").await;
        input.instruments[0].instrument_id = 999_999_999;
        assert!(matches!(
            register_deployment(&pool, &input).await,
            Err(DeploymentError::MissingMetadata(_))
        ));
        let (_, _, _, mut input) = seed_registration(&pool, "PAPER").await;
        input.assets.clear();
        assert!(matches!(
            register_deployment(&pool, &input).await,
            Err(DeploymentError::MissingBaseline(_))
        ));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn an_account_alias_cannot_be_registered_twice() {
        let pool = isolated_pool().await;
        let (_, account, _, input) = seed_registration(&pool, "PAPER").await;
        register_deployment(&pool, &input).await.unwrap();
        use crate::strategies::test_support as t;
        let other = t::seed_strategy(&pool).await;
        let direct = sqlx::query(
            "INSERT INTO colo_deployment (id, strategy_code, account_id, venue, environment, \
                external_account_ref, authority_secs, market_data_max_age_secs, allocation_quote_asset, \
                allocation_max_notional) \
             VALUES ($1, $2, $3, 'BINANCE', 'PAPER', 'x', 5, 2, 'USDT', 1000)",
        )
        .bind(Uuid::new_v4())
        .bind(other)
        .bind(account)
        .execute(&pool)
        .await;
        assert!(direct.is_err());
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn bootstrap_publishes_a_new_policy_revision_when_limits_change() {
        use crate::strategies::test_support as t;
        let pool = isolated_pool().await;
        let (code, _, _, input) = seed_registration(&pool, "PAPER").await;
        let deployment = register_deployment(&pool, &input).await.unwrap();
        let state = t::state(pool.clone());
        let first = bootstrap(&state, &deployment).await.unwrap();
        assert_eq!(first.baseline.sequence, 1);
        sqlx::query("UPDATE strategy SET max_order_quantity = 5 WHERE code = $1")
            .bind(&code)
            .execute(&pool)
            .await
            .unwrap();
        let second = bootstrap(&state, &deployment).await.unwrap();
        assert!(second.baseline.sequence > first.baseline.sequence);
        assert!(second.limits.values().any(|l| l.max_order_qty == Some(5.0)));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn runtime_acknowledgements_are_recorded_and_never_move_back() {
        use colo_link::v2::ControlAckStage::*;
        let pool = isolated_pool().await;
        let (_, _, _, input) = seed_registration(&pool, "PAPER").await;
        let deployment = register_deployment(&pool, &input).await.unwrap();
        let session = acquire_session(&pool, deployment.id, Uuid::new_v4()).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string();
        let kill_id = format!("kill-{}", &suffix[..12]);
        apply_command(
            &pool,
            &ControlCommand {
                deployment_id: deployment.id,
                epoch: session.epoch,
                command_id: kill_id.clone(),
                kind: CommandKind::Kill,
            },
        )
        .await
        .unwrap();
        let stage = || async {
            get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap().kill_ack_stage
        };
        assert_eq!(stage().await, None);
        assert!(!stalled_kills(&pool).await.unwrap().contains(&deployment.strategy_code), "just sent");
        sqlx::query("UPDATE colo_deployment_command SET created_at = now() - interval '60 seconds' WHERE command_id = $1")
            .bind(&kill_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(stalled_kills(&pool).await.unwrap().contains(&deployment.strategy_code), "unacknowledged for a minute");
        assert!(record_control_ack(&pool, deployment.id, session.epoch, &kill_id, CancelsSent).await.unwrap());
        assert_eq!(stage().await.as_deref(), Some("CANCELS_SENT"));
        assert!(!record_control_ack(&pool, deployment.id, session.epoch, &kill_id, Received).await.unwrap(), "late ACK");
        assert!(!record_control_ack(&pool, deployment.id, session.epoch - 1, &kill_id, ConfirmedClear).await.unwrap(), "stale epoch");
        assert!(!record_control_ack(&pool, deployment.id, session.epoch, "kill-other", ConfirmedClear).await.unwrap());
        assert_eq!(stage().await.as_deref(), Some("CANCELS_SENT"));
        assert!(stalled_kills(&pool).await.unwrap().contains(&deployment.strategy_code), "not clear after a minute");
        assert!(record_control_ack(&pool, deployment.id, session.epoch, &kill_id, ConfirmedClear).await.unwrap());
        assert_eq!(stage().await.as_deref(), Some("CONFIRMED_CLEAR"));
        assert!(!stalled_kills(&pool).await.unwrap().contains(&deployment.strategy_code));

        let published = deployment.policy_revision as u64;
        record_policy_ack(&pool, deployment.id, published + 5).await.unwrap();
        record_policy_ack(&pool, deployment.id, published).await.unwrap();
        record_policy_ack(&pool, deployment.id, published.saturating_sub(1).max(1)).await.unwrap();
        record_recovery_status(&pool, deployment.id, false, &["order c-1 has no venue evidence".into()]).await.unwrap();
        let reloaded = get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap();
        assert_eq!(reloaded.acked_policy_revision, Some(published as i64), "unpublished and older revisions ignored");
        assert_eq!(reloaded.recovery_ready, Some(false));
        assert_eq!(reloaded.recovery_missing.as_deref(), Some("order c-1 has no venue evidence"));
        assert!(reloaded.recovery_reported_at.is_some());
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_rebaseline_is_recorded_and_acknowledged_and_shortfalls_are_stored() {
        use colo_link::v2::ControlAckStage::*;
        let pool = isolated_pool().await;
        let (_, _, _, input) = seed_registration(&pool, "PAPER").await;
        let deployment = register_deployment(&pool, &input).await.unwrap();
        let session = acquire_session(&pool, deployment.id, Uuid::new_v4()).await.unwrap();

        record_inventory_status(&pool, deployment.id, &["USDT is 500 short".into(), "BTC is 1 short".into()])
            .await
            .unwrap();
        let reloaded = get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap();
        assert_eq!(reloaded.inventory_shortfall.as_deref(), Some("USDT is 500 short\nBTC is 1 short"));
        assert!(reloaded.inventory_reported_at.is_some());

        let receipt = record_rebaseline_for_strategy(&pool, &deployment.strategy_code).await.unwrap().unwrap();
        assert_eq!(receipt.deployment_id, deployment.id);
        assert_eq!(receipt.epoch, session.epoch);
        assert!(record_control_ack(&pool, deployment.id, session.epoch, &receipt.command_id, Received).await.unwrap());
        assert!(record_control_ack(&pool, deployment.id, session.epoch, &receipt.command_id, ConfirmedClear).await.unwrap());
        record_inventory_status(&pool, deployment.id, &[]).await.unwrap();
        let reloaded = get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap();
        assert_eq!(reloaded.inventory_shortfall, None);
        assert_eq!(reloaded.kill_epoch, None, "a rebaseline is not a kill");
        assert!(record_rebaseline_for_strategy(&pool, "NO-SUCH-STRATEGY").await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn kill_commands_are_epoch_exact_and_survive_a_reload() {
        let pool = isolated_pool().await;
        let (_, _, _, input) = seed_registration(&pool, "PAPER").await;
        let deployment = register_deployment(&pool, &input).await.unwrap();
        let session = acquire_session(&pool, deployment.id, Uuid::new_v4()).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string();
        let kill_id = format!("kill-{}", &suffix[..12]);
        let enable_id = format!("enable-{}", &suffix[..12]);
        let stale = apply_command(
            &pool,
            &ControlCommand {
                deployment_id: deployment.id,
                epoch: session.epoch - 1,
                command_id: format!("stale-{}", &suffix[..12]),
                kind: CommandKind::Kill,
            },
        )
        .await;
        assert!(matches!(stale, Err(DeploymentError::StaleEpoch { .. })));
        let kill = apply_command(
            &pool,
            &ControlCommand {
                deployment_id: deployment.id,
                epoch: session.epoch,
                command_id: kill_id.clone(),
                kind: CommandKind::Kill,
            },
        )
        .await
        .unwrap();
        let reloaded = get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap();
        assert_eq!(reloaded.kill_epoch, Some(session.epoch as i64));
        assert_eq!(reloaded.kill_command_id.as_deref(), Some(kill_id.as_str()));
        assert!(kill.acknowledges(&ControlCommand {
            deployment_id: deployment.id,
            epoch: session.epoch,
            command_id: kill_id,
            kind: CommandKind::Kill,
        }));
        apply_command(
            &pool,
            &ControlCommand {
                deployment_id: deployment.id,
                epoch: session.epoch,
                command_id: enable_id,
                kind: CommandKind::Enable { stopped_epoch: session.epoch },
            },
        )
        .await
        .unwrap();
        let reloaded = get_deployment(&pool, &deployment.strategy_code, deployment.id).await.unwrap();
        assert_eq!(reloaded.kill_epoch, None);
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn ownership_cannot_be_replaced_until_the_former_owner_is_verified_stopped() {
        let pool = isolated_pool().await;
        let (_, _, _, input) = seed_registration(&pool, "PAPER").await;
        let deployment = register_deployment(&pool, &input).await.unwrap();
        let first = acquire_session(&pool, deployment.id, Uuid::new_v4()).await.unwrap();
        assert_eq!(first.epoch, 1);
        assert!(matches!(
            acquire_session(&pool, deployment.id, Uuid::new_v4()).await,
            Err(DeploymentError::OwnershipNotVerified)
        ));
    }

}
