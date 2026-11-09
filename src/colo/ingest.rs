//! Durable event inbox and one economic trade identity shared by the runtime
//! reporter and the independent venue follow. ACK means committed, not queued.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::warn;
use uuid::Uuid;

use colo_link::v2::{EventBatch, EventRecord};
use gateway::contract::ExternalFill;
use gateway::runtime::{AccountIdentity, RuntimeEvent, Side, Trade, TradeKey, VenueEvent};

use crate::admin::AdminError;
use crate::app_state::AppState;
use super::deployment::Deployment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    NotFound,
    Retired,
    Suspended(String),
    SequenceGap { expected: u64, got: u64 },
    ConflictingDuplicate(String),
    NoDeployment,
    MissingInstrument(String),
    Invalid(String),
    Database(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "deployment not found"),
            Self::Retired => write!(f, "deployment is retired"),
            Self::Suspended(reason) => write!(f, "deployment ingest is suspended: {reason}"),
            Self::SequenceGap { expected, got } => write!(f, "sequence gap: expected {expected}, got {got}"),
            Self::ConflictingDuplicate(msg) => write!(f, "conflicting duplicate: {msg}"),
            Self::NoDeployment => write!(f, "no deployment owns this account"),
            Self::MissingInstrument(msg) => write!(f, "missing instrument metadata: {msg}"),
            Self::Invalid(msg) => write!(f, "invalid event: {msg}"),
            Self::Database(msg) => write!(f, "database error: {msg}"),
        }
    }
}

impl std::error::Error for IngestError {}

fn db(e: sqlx::Error) -> IngestError {
    IngestError::Database(e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableAck {
    pub deployment_id: Uuid,
    pub through_sequence: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TradeReceipt {
    pub key: TradeKey,
    pub deployment_id: Uuid,
    pub inserted: bool,
    pub fee_enriched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainProgress {
    pub processed: usize,
    pub last_sequence: Option<u64>,
}

/// Store a contiguous batch durably. Identical replays are idempotent;
/// a different payload for an already stored sequence suspends the deployment.
pub async fn accept_batch(
    pool: &PgPool,
    deployment: &Deployment,
    batch: &EventBatch,
) -> Result<DurableAck, IngestError> {
    batch
        .validate()
        .map_err(|e| IngestError::Invalid(e.to_string()))?;
    let mut tx = pool.begin().await.map_err(db)?;
    let row = sqlx::query(
        "SELECT status, ingest_suspended_reason FROM colo_deployment WHERE id = $1 FOR UPDATE",
    )
    .bind(deployment.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(IngestError::NotFound)?;
    if row.get::<String, _>("status") != "ACTIVE" {
        return Err(IngestError::Retired);
    }
    if let Some(reason) = row.get::<Option<String>, _>("ingest_suspended_reason") {
        return Err(IngestError::Suspended(reason));
    }
    let max: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence), 0) FROM colo_event_inbox WHERE deployment_id = $1",
    )
    .bind(deployment.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let max = max.max(0) as u64;
    let first = batch.first_sequence;
    if first > max + 1 {
        return Err(IngestError::SequenceGap { expected: max + 1, got: first });
    }

    for record in &batch.records {
        if record.sequence <= max {
            let existing: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT payload FROM colo_event_inbox WHERE deployment_id = $1 AND sequence = $2",
            )
            .bind(deployment.id)
            .bind(record.sequence as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
            let incoming = serde_json::to_value(record)
                .map_err(|e| IngestError::Invalid(e.to_string()))?;
            if existing.as_ref() != Some(&incoming) {
                sqlx::query(
                    "UPDATE colo_deployment SET ingest_suspended_at = now(), \
                        ingest_suspended_reason = $2, updated_at = now() WHERE id = $1",
                )
                .bind(deployment.id)
                .bind(format!("sequence {} replayed with different payload", record.sequence))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
                tx.commit().await.map_err(db)?;
                return Err(IngestError::ConflictingDuplicate(format!(
                    "sequence {}",
                    record.sequence
                )));
            }
            continue;
        }
        sqlx::query(
            "INSERT INTO colo_event_inbox (deployment_id, sequence, payload) VALUES ($1, $2, $3)",
        )
        .bind(deployment.id)
        .bind(record.sequence as i64)
        .bind(serde_json::to_value(record).map_err(|e| IngestError::Invalid(e.to_string()))?)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(DurableAck { deployment_id: deployment.id, through_sequence: batch.last_sequence() })
}

/// Process committed records in sequence order. Booking is idempotent, so a
/// crash between booking and marking processed replays safely.
/// How often accepted-but-unbooked inbox events are retried.
pub const DRAIN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Active deployments with inbox events not yet booked, ingest not suspended.
pub async fn pending_deployments(pool: &PgPool) -> Result<Vec<Uuid>, IngestError> {
    sqlx::query_scalar(
        "SELECT DISTINCT i.deployment_id FROM colo_event_inbox i \
         JOIN colo_deployment d ON d.id = i.deployment_id \
         WHERE i.processed_at IS NULL AND d.status = 'ACTIVE' AND d.ingest_suspended_reason IS NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(db)
}

/// Retries booking accepted events in the background. The runtime's events
/// are acknowledged once stored, so a booking that failed (no adapter yet, a
/// database error) would otherwise wait for the deployment's next batch.
pub async fn drain_loop(state: AppState) {
    let mut tick = tokio::time::interval(DRAIN_EVERY);
    loop {
        tick.tick().await;
        let pending = match pending_deployments(state.pool()).await {
            Ok(pending) => pending,
            Err(e) => {
                warn!(error = %e, "co-located: could not list undrained inboxes");
                continue;
            }
        };
        for deployment_id in pending {
            match drain_inbox(&state, deployment_id).await {
                Ok(progress) if progress.processed > 0 => {
                    tracing::info!(deployment = %deployment_id, booked = progress.processed, "co-located: inbox drained on retry");
                }
                Ok(_) => {}
                Err(e) => warn!(deployment = %deployment_id, error = %e, "co-located: inbox drain failed; will retry"),
            }
        }
    }
}

pub async fn drain_inbox(
    state: &AppState,
    deployment_id: Uuid,
) -> Result<DrainProgress, IngestError> {
    let guard = state.colo().drain_guard(deployment_id);
    let _draining = guard.lock().await;
    let deployment = sqlx::query(
        "SELECT strategy_code, account_id, venue, environment, external_account_ref, status, ingest_suspended_reason \
         FROM colo_deployment WHERE id = $1",
    )
    .bind(deployment_id)
    .fetch_optional(state.pool())
    .await
    .map_err(db)?
    .ok_or(IngestError::NotFound)?;
    if deployment.get::<String, _>("status") != "ACTIVE" {
        return Err(IngestError::Retired);
    }
    if let Some(reason) = deployment.get::<Option<String>, _>("ingest_suspended_reason") {
        return Err(IngestError::Suspended(reason));
    }
    let identity = AccountIdentity {
        account_id: deployment.get("account_id"),
        venue: deployment.get("venue"),
        environment: deployment.get("environment"),
        external_account_ref: deployment.get("external_account_ref"),
    };
    let strategy_code: String = deployment.get("strategy_code");
    let rows = sqlx::query(
        "SELECT sequence, payload FROM colo_event_inbox \
         WHERE deployment_id = $1 AND processed_at IS NULL ORDER BY sequence",
    )
    .bind(deployment_id)
    .fetch_all(state.pool())
    .await
    .map_err(db)?;
    let mut processed = 0usize;
    let mut last_sequence = None;
    for row in rows {
        let sequence = row.get::<i64, _>("sequence") as u64;
        let payload: serde_json::Value = row.get("payload");
        let record: EventRecord = serde_json::from_value(payload)
            .map_err(|e| IngestError::Invalid(e.to_string()))?;
        if let RuntimeEvent::Venue { account: _, event: VenueEvent::Trade(trade) } = &record.event {
            observe_trade(state, &identity, trade).await?;
            {
                let strategy = state
                    .strategies()
                    .by_code(&strategy_code)
                    .ok_or_else(|| IngestError::Invalid(format!("unknown strategy {strategy_code}")))?;
                let adapter = state
                    .colo()
                    .adapter(&strategy_code)
                    .ok_or_else(|| IngestError::Invalid("no venue adapter for booking".into()))?;
                let fill = ExternalFill {
                    trade_id: trade.key.trade_id.clone(),
                    venue_order_id: trade.venue_order_id.clone(),
                    client_order_id: trade.client_order_id.clone(),
                    symbol: trade.key.symbol.clone(),
                    buy: trade.side == Side::Buy,
                    qty: trade.quantity,
                    price: trade.price,
                    fee_amount: trade.fee.as_ref().map(|f| f.amount),
                    fee_currency: trade.fee.as_ref().map(|f| f.currency.clone()),
                    executed_at: Some(trade.executed_at),
                };
                super::booking::book_fill(state, &strategy, &adapter, &fill)
                    .await
                    .map_err(IngestError::Invalid)?;
            }
        }
        sqlx::query(
            "UPDATE colo_event_inbox SET processed_at = now() \
             WHERE deployment_id = $1 AND sequence = $2 AND processed_at IS NULL",
        )
        .bind(deployment_id)
        .bind(sequence as i64)
        .execute(state.pool())
        .await
        .map_err(db)?;
        processed += 1;
        last_sequence = Some(sequence);
    }
    Ok(DrainProgress { processed, last_sequence })
}

/// One economic fill. Runtime and independent follow both call this; the
/// unique key makes the second arrival a no-op (or a fee enrichment).
pub async fn observe_trade(
    state: &AppState,
    identity: &AccountIdentity,
    trade: &Trade,
) -> Result<TradeReceipt, IngestError> {
    validate_trade(identity, trade)?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    let deployment: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM colo_deployment WHERE account_id = $1 AND venue = $2 AND environment = $3 \
         AND status = 'ACTIVE' ORDER BY created_at LIMIT 1",
    )
    .bind(identity.account_id)
    .bind(&identity.venue)
    .bind(&identity.environment)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    let deployment_id = deployment.ok_or(IngestError::NoDeployment)?;

    let existing = sqlx::query(
        "SELECT id, side, quantity::double precision AS quantity, price::double precision AS price, \
                venue_order_id, client_order_id, executed_at, fee_amount::double precision AS fee_amount, \
                fee_currency FROM colo_trade_ledger \
         WHERE venue = $1 AND account_id = $2 AND environment = $3 AND symbol = $4 AND trade_id = $5",
    )
    .bind(&trade.key.venue)
    .bind(trade.key.account_id)
    .bind(&trade.key.environment)
    .bind(&trade.key.symbol)
    .bind(&trade.key.trade_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;

    if let Some(row) = existing {
        let same = row.get::<String, _>("side") == side_str(trade.side)
            && (row.get::<f64, _>("quantity") - trade.quantity).abs() < 1e-12
            && (row.get::<f64, _>("price") - trade.price).abs() < 1e-12
            && row.get::<String, _>("venue_order_id") == trade.venue_order_id
            && row.get::<String, _>("client_order_id") == trade.client_order_id;
        if !same {
            suspend(&mut tx, deployment_id, "economic trade identity payload changed").await?;
            tx.commit().await.map_err(db)?;
            return Err(IngestError::ConflictingDuplicate(trade.key.trade_id.clone()));
        }
        let existing_fee: Option<f64> = row.get("fee_amount");
        let existing_currency: Option<String> = row.get("fee_currency");
        let mut enriched = false;
        if let Some(fee) = &trade.fee {
            match (existing_fee, existing_currency.as_deref()) {
                (None, None) => {
                    sqlx::query(
                        "UPDATE colo_trade_ledger SET fee_amount = $2, fee_currency = $3 WHERE id = $1",
                    )
                    .bind(row.get::<Uuid, _>("id"))
                    .bind(fee.amount)
                    .bind(&fee.currency)
                    .execute(&mut *tx)
                    .await
                    .map_err(db)?;
                    move_asset(
                        &mut tx,
                        deployment_id,
                        identity,
                        &trade.key.symbol,
                        &trade.key.trade_id,
                        &fee.currency,
                        "FEE",
                        -fee.amount,
                    )
                    .await?;
                    enriched = true;
                }
                (Some(amount), Some(currency))
                    if (amount - fee.amount).abs() < 1e-12 && currency == fee.currency => {}
                _ => {
                    suspend(&mut tx, deployment_id, "fee revision conflicts with booked trade").await?;
                    tx.commit().await.map_err(db)?;
                    return Err(IngestError::ConflictingDuplicate(trade.key.trade_id.clone()));
                }
            }
        }
        tx.commit().await.map_err(db)?;
        return Ok(TradeReceipt {
            key: trade.key.clone(),
            deployment_id,
            inserted: false,
            fee_enriched: enriched,
        });
    }

    let inserted = sqlx::query(
        "INSERT INTO colo_trade_ledger \
         (id, deployment_id, venue, account_id, environment, symbol, trade_id, venue_order_id, \
          client_order_id, side, quantity, price, executed_at, fee_amount, fee_currency, provenance) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
         ON CONFLICT (venue, account_id, environment, symbol, trade_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(deployment_id)
    .bind(&trade.key.venue)
    .bind(trade.key.account_id)
    .bind(&trade.key.environment)
    .bind(&trade.key.symbol)
    .bind(&trade.key.trade_id)
    .bind(&trade.venue_order_id)
    .bind(&trade.client_order_id)
    .bind(side_str(trade.side))
    .bind(trade.quantity)
    .bind(trade.price)
    .bind(trade.executed_at)
    .bind(trade.fee.as_ref().map(|f| f.amount))
    .bind(trade.fee.as_ref().map(|f| f.currency.as_str()))
    .bind(format!("{:?}", trade.provenance).to_lowercase())
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await.map_err(db)?;
        return Box::pin(observe_trade(state, identity, trade)).await;
    }

    let metadata = sqlx::query(
        "SELECT i.currency, i.contract_size::double precision AS contract_size, bi.native_id \
         FROM broker_instrument bi JOIN instrument i ON i.id = bi.instrument_id \
         WHERE bi.broker_symbol = $1 AND bi.broker_code = $2",
    )
    .bind(&trade.key.symbol)
    .bind(&trade.key.venue)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(|| IngestError::MissingInstrument(trade.key.symbol.clone()))?;
    let base: String = metadata
        .get::<Option<String>, _>("native_id")
        .unwrap_or_else(|| trade.key.symbol.clone());
    let quote: String = metadata.get("currency");
    let contract: f64 = metadata.get("contract_size");
    let quantity = trade.quantity * contract;
    let notional = quantity * trade.price;
    let (base_delta, quote_delta) = match trade.side {
        Side::Buy => (quantity, -notional),
        Side::Sell => (-quantity, notional),
    };
    for (asset, kind, amount) in [
        (base.as_str(), "BASE", base_delta),
        (quote.as_str(), "QUOTE", quote_delta),
    ] {
        move_asset(
            &mut tx,
            deployment_id,
            identity,
            &trade.key.symbol,
            &trade.key.trade_id,
            asset,
            kind,
            amount,
        )
        .await?;
    }
    if let Some(fee) = &trade.fee {
        move_asset(
            &mut tx,
            deployment_id,
            identity,
            &trade.key.symbol,
            &trade.key.trade_id,
            &fee.currency,
            "FEE",
            -fee.amount,
        )
        .await?;
    }
    tx.commit().await.map_err(db)?;
    Ok(TradeReceipt {
        key: trade.key.clone(),
        deployment_id,
        inserted: true,
        fee_enriched: false,
    })
}

fn validate_trade(identity: &AccountIdentity, trade: &Trade) -> Result<(), IngestError> {
    if trade.key.venue != identity.venue
        || trade.key.account_id != identity.account_id
        || trade.key.environment != identity.environment
    {
        return Err(IngestError::Invalid("trade identity does not match the account".into()));
    }
    if !trade.quantity.is_finite() || trade.quantity <= 0.0 || !trade.price.is_finite() || trade.price <= 0.0 {
        return Err(IngestError::Invalid("quantity and price must be finite and positive".into()));
    }
    if trade.key.trade_id.is_empty() || trade.key.symbol.is_empty() {
        return Err(IngestError::Invalid("trade identity is incomplete".into()));
    }
    Ok(())
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

async fn suspend(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deployment_id: Uuid,
    reason: &str,
) -> Result<(), IngestError> {
    sqlx::query(
        "UPDATE colo_deployment SET ingest_suspended_at = now(), ingest_suspended_reason = $2, \
            updated_at = now() WHERE id = $1",
    )
    .bind(deployment_id)
    .bind(reason)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn move_asset(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    deployment_id: Uuid,
    identity: &AccountIdentity,
    symbol: &str,
    trade_id: &str,
    asset: &str,
    kind: &str,
    amount: f64,
) -> Result<(), IngestError> {
    let inserted = sqlx::query(
        "INSERT INTO colo_asset_event \
         (id, deployment_id, venue, account_id, environment, symbol, trade_id, asset, entry_kind, amount) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (venue, account_id, environment, symbol, trade_id, asset, entry_kind) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(deployment_id)
    .bind(&identity.venue)
    .bind(identity.account_id)
    .bind(&identity.environment)
    .bind(symbol)
    .bind(trade_id)
    .bind(asset)
    .bind(kind)
    .bind(amount)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    if inserted.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO colo_asset_balance (account_id, asset, quantity) VALUES ($1, $2, $3) \
         ON CONFLICT (account_id, asset) DO UPDATE SET \
            quantity = colo_asset_balance.quantity + EXCLUDED.quantity, updated_at = now()",
    )
    .bind(identity.account_id)
    .bind(asset)
    .bind(amount)
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    Ok(())
}

/// Apply an audited baseline adjustment to a stopped deployment's native asset
/// balance. Running deployments cannot rewrite their opening inventory.
pub async fn adjust_balance(
    state: &AppState,
    deployment_id: Uuid,
    asset: &str,
    amount: f64,
    reason: &str,
    operator: &str,
) -> Result<(), IngestError> {
    if asset.trim().is_empty() || !amount.is_finite() || reason.trim().is_empty() || operator.trim().is_empty() {
        return Err(IngestError::Invalid("asset, finite amount, reason and operator are required".into()));
    }
    let mut tx = state.pool().begin().await.map_err(db)?;
    let stopped: bool = sqlx::query_scalar(
        "SELECT status = 'RETIRED' OR kill_epoch IS NOT NULL FROM colo_deployment WHERE id = $1 FOR UPDATE",
    )
    .bind(deployment_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or(IngestError::NotFound)?;
    if !stopped {
        return Err(IngestError::Invalid("only a stopped deployment may change its baseline".into()));
    }
    sqlx::query(
        "INSERT INTO colo_baseline_adjustment (id, deployment_id, asset, amount, reason, operator) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(deployment_id)
    .bind(asset)
    .bind(amount)
    .bind(reason)
    .bind(operator)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let account: Uuid = sqlx::query_scalar("SELECT account_id FROM colo_deployment WHERE id = $1")
        .bind(deployment_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query(
        "INSERT INTO colo_asset_balance (account_id, asset, quantity) VALUES ($1, $2, $3) \
         ON CONFLICT (account_id, asset) DO UPDATE SET quantity = colo_asset_balance.quantity + EXCLUDED.quantity, updated_at = now()",
    )
    .bind(account)
    .bind(asset)
    .bind(amount)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    sqlx::query(
        "INSERT INTO colo_deployment_asset_baseline (deployment_id, asset, quantity) VALUES ($1, $2, $3) \
         ON CONFLICT (deployment_id, asset) DO UPDATE SET quantity = colo_deployment_asset_baseline.quantity + EXCLUDED.quantity",
    )
    .bind(deployment_id)
    .bind(asset)
    .bind(amount)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AdjustmentRequest {
    pub asset: String,
    pub amount: f64,
    pub reason: String,
}

#[utoipa::path(
    post, path = "/admin/strategies/{code}/deployments/{id}/adjustments", tag = "admin",
    params(("code" = String, Path, description = "Strategy code"), ("id" = Uuid, Path, description = "Deployment id")),
    request_body = AdjustmentRequest,
    responses((status = 204, description = "Adjusted"), (status = 400, description = "Deployment not stopped or invalid"), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn adjustment(
    State(state): State<AppState>,
    Path((code, id)): Path<(String, Uuid)>,
    axum::extract::Extension(auth): axum::extract::Extension<crate::auth::AuthContext>,
    Json(request): Json<AdjustmentRequest>,
) -> Result<StatusCode, AdminError> {
    let deployment = super::deployment::get_deployment(state.pool(), &code, id)
        .await
        .map_err(|e| AdminError::new(StatusCode::NOT_FOUND, e.to_string()))?;
    adjust_balance(
        &state,
        deployment.id,
        &request.asset,
        request.amount,
        &request.reason,
        &auth.principal_id.to_string(),
    )
    .await
    .map_err(|e| AdminError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn trade_from_external(
    identity: &AccountIdentity,
    fill: &gateway::contract::ExternalFill,
    executed_at: DateTime<Utc>,
) -> Trade {
    Trade {
        key: TradeKey {
            venue: identity.venue.clone(),
            account_id: identity.account_id,
            environment: identity.environment.clone(),
            symbol: fill.symbol.clone(),
            trade_id: fill.trade_id.clone(),
        },
        venue_order_id: fill.venue_order_id.clone(),
        client_order_id: fill.client_order_id.clone(),
        side: if fill.buy { Side::Buy } else { Side::Sell },
        quantity: fill.qty,
        price: fill.price,
        executed_at,
        fee: None,
        provenance: gateway::runtime::TradeProvenance::Stream,
    }
}

pub async fn identity_for_strategy(
    pool: &PgPool,
    strategy_code: &str,
) -> Result<Option<AccountIdentity>, IngestError> {
    let row = sqlx::query(
        "SELECT a.id AS account_id, a.external_account_ref, s.follow_environment          FROM strategy s          JOIN strategy_connection sc ON sc.strategy_code = s.code AND sc.broker_code = 'BINANCE'          JOIN account a ON a.id = sc.account_id          WHERE s.code = $1 AND s.mode = 'COLOCATED'",
    )
    .bind(strategy_code)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let Some(environment) = row.get::<Option<String>, _>("follow_environment") else {
        return Ok(None);
    };
    Ok(Some(AccountIdentity {
        account_id: row.get("account_id"),
        venue: "BINANCE".into(),
        environment,
        external_account_ref: row.get("external_account_ref"),
    }))
}

/// Follow-path bridge: ignore fills for accounts that have no registered
/// deployment; registered accounts share the same economic identity as runtime.
pub async fn observe_external_fill(
    state: &AppState,
    identity: &AccountIdentity,
    fill: &gateway::contract::ExternalFill,
) -> Result<Option<TradeReceipt>, IngestError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM colo_deployment WHERE account_id = $1 AND venue = $2 AND environment = $3 AND status = 'ACTIVE')",
    )
    .bind(identity.account_id)
    .bind(&identity.venue)
    .bind(&identity.environment)
    .fetch_one(state.pool())
    .await
    .map_err(db)?;
    if !exists {
        return Ok(None);
    }
    let trade = trade_from_external(identity, fill, Utc::now());
    observe_trade(state, identity, &trade).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::contract::ExternalFill;

    fn identity() -> AccountIdentity {
        AccountIdentity {
            account_id: Uuid::new_v4(),
            venue: "BINANCE".into(),
            environment: "LIVE".into(),
            external_account_ref: "sub".into(),
        }
    }

    fn fill() -> ExternalFill {
        ExternalFill {
            trade_id: "7".into(),
            venue_order_id: "9".into(),
            client_order_id: "c".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            qty: 1.0,
            price: 100.0,
            fee_amount: None,
            fee_currency: None,
            executed_at: None,
        }
    }

    #[test]
    fn the_follow_bridge_keeps_the_economic_key() {
        let id = identity();
        let trade = trade_from_external(&id, &fill(), Utc::now());
        assert_eq!(trade.key.account_id, id.account_id);
        assert_eq!(trade.key.symbol, "BTCUSDT");
        assert_eq!(trade.key.trade_id, "7");
        assert_eq!(trade.side, Side::Buy);
    }

    #[test]
    fn a_mismatched_identity_is_invalid() {
        let mut trade = trade_from_external(&identity(), &fill(), Utc::now());
        trade.key.account_id = Uuid::new_v4();
        assert!(matches!(validate_trade(&identity(), &trade), Err(IngestError::Invalid(_))));
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

    async fn seeded(pool: &PgPool) -> (Deployment, AccountIdentity, String, String) {
        use crate::colo::deployment as d;
        use crate::strategies::test_support as t;
        let code: String = t::seed_strategy(pool).await;
        let (symbol, instrument_id) = t::seed_pair(pool, &["BINANCE"]).await;
        let portfolio: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
            .bind(&code).fetch_one(pool).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string();
        let connection: String = sqlx::query_scalar(
            "SELECT code FROM broker_connection WHERE broker_code = 'BINANCE' AND environment = 'PAPER' LIMIT 1",
        ).fetch_one(pool).await.unwrap();
        let account = Uuid::new_v4();
        let external = format!("ingest-{}", &suffix[..12]);
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
             VALUES ($1, $2, $3, $4, 'ACTIVE', $5)",
        )
        .bind(account)
        .bind(format!("ingest-account-{suffix}"))
        .bind(&connection)
        .bind(&external)
        .bind(portfolio)
        .execute(pool).await.unwrap();
        let quote: String = sqlx::query_scalar("SELECT currency FROM instrument WHERE id = $1")
            .bind(instrument_id).fetch_one(pool).await.unwrap();
        sqlx::query("UPDATE strategy SET mode = 'COLOCATED', follow_environment = 'PAPER' WHERE code = $1")
            .bind(&code).execute(pool).await.unwrap();
        let input = d::DeploymentRegistration {
            strategy_code: code.clone(),
            account_id: account,
            venue: "BINANCE".into(),
            environment: "PAPER".into(),
            external_account_ref: external.clone(),
            authority_secs: 5,
            market_data_max_age_secs: 2,
            heartbeat_secs: Some(1),
            allocation_quote_asset: quote.clone(),
            allocation_max_notional: 10_000.0,
            allocation_max_position_qty: Some(10.0),
            allocation_fee_allowance_bps: 10.0,
            instruments: vec![d::InstrumentBaseline { instrument_id, quantity: 0.0, cost_basis: 0.0 }],
            assets: vec![d::AssetHolding { asset: quote.clone(), quantity: 1_000.0 }],
        };
        let deployment = d::register_deployment(pool, &input).await.unwrap();
        (
            deployment,
            AccountIdentity {
                account_id: account,
                venue: "BINANCE".into(),
                environment: "PAPER".into(),
                external_account_ref: external,
            },
            symbol,
            quote,
        )
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_deployment_with_unbooked_events_is_pending_until_booked() {
        let pool = isolated_pool().await;
        let (deployment, identity, _, _) = seeded(&pool).await;
        assert!(!pending_deployments(&pool).await.unwrap().contains(&deployment.id));
        let batch = EventBatch {
            first_sequence: 1,
            records: vec![EventRecord {
                sequence: 1,
                event: RuntimeEvent::Health {
                    account: identity,
                    state: gateway::runtime::StreamState::Live,
                    observed_at: Utc::now(),
                },
            }],
        };
        accept_batch(&pool, &deployment, &batch).await.unwrap();
        assert!(pending_deployments(&pool).await.unwrap().contains(&deployment.id), "accepted, not booked");
        sqlx::query("UPDATE colo_deployment SET ingest_suspended_reason = 'x' WHERE id = $1")
            .bind(deployment.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!pending_deployments(&pool).await.unwrap().contains(&deployment.id), "suspended ingest is left alone");
        sqlx::query("UPDATE colo_deployment SET ingest_suspended_reason = NULL WHERE id = $1")
            .bind(deployment.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE colo_event_inbox SET processed_at = now() WHERE deployment_id = $1")
            .bind(deployment.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!pending_deployments(&pool).await.unwrap().contains(&deployment.id));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_replayed_batch_is_idempotent_and_a_changed_payload_suspends() {
        let pool = isolated_pool().await;
        let (deployment, identity, _, _) = seeded(&pool).await;
        let event = RuntimeEvent::Health {
            account: identity,
            state: gateway::runtime::StreamState::Live,
            observed_at: Utc::now(),
        };
        let batch = EventBatch {
            first_sequence: 1,
            records: vec![EventRecord { sequence: 1, event: event.clone() }],
        };
        assert_eq!(accept_batch(&pool, &deployment, &batch).await.unwrap().through_sequence, 1);
        assert_eq!(accept_batch(&pool, &deployment, &batch).await.unwrap().through_sequence, 1);
        let gap = EventBatch {
            first_sequence: 3,
            records: vec![EventRecord { sequence: 3, event }],
        };
        assert!(matches!(
            accept_batch(&pool, &deployment, &gap).await,
            Err(IngestError::SequenceGap { .. })
        ));
        let conflict = EventBatch {
            first_sequence: 1,
            records: vec![EventRecord {
                sequence: 1,
                event: RuntimeEvent::Health {
                    account: AccountIdentity {
                        account_id: Uuid::new_v4(),
                        venue: "BINANCE".into(),
                        environment: "PAPER".into(),
                        external_account_ref: "x".into(),
                    },
                    state: gateway::runtime::StreamState::Closed,
                    observed_at: Utc::now(),
                },
            }],
        };
        assert!(matches!(
            accept_batch(&pool, &deployment, &conflict).await,
            Err(IngestError::ConflictingDuplicate(_))
        ));
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn one_trade_moves_assets_once_and_a_later_fee_enriches_it() {
        use crate::strategies::test_support as t;
        let pool = isolated_pool().await;
        let (deployment, identity, symbol, quote) = seeded(&pool).await;
        let state = t::state(pool.clone());
        let mut trade = Trade {
            key: TradeKey {
                venue: "BINANCE".into(),
                account_id: identity.account_id,
                environment: "PAPER".into(),
                symbol: symbol.clone(),
                trade_id: "7".into(),
            },
            venue_order_id: "9".into(),
            client_order_id: "c".into(),
            side: Side::Buy,
            quantity: 1.0,
            price: 100.0,
            executed_at: Utc::now(),
            fee: None,
            provenance: gateway::runtime::TradeProvenance::History,
        };
        assert!(observe_trade(&state, &identity, &trade).await.unwrap().inserted);
        assert!(!observe_trade(&state, &identity, &trade).await.unwrap().inserted);
        let ledger: i64 = sqlx::query_scalar("SELECT count(*) FROM colo_trade_ledger WHERE deployment_id = $1")
            .bind(deployment.id).fetch_one(&pool).await.unwrap();
        assert_eq!(ledger, 1);
        trade.fee = Some(gateway::runtime::Fee { amount: 0.1, currency: "BNB".into() });
        assert!(observe_trade(&state, &identity, &trade).await.unwrap().fee_enriched);
        let quote_balance: f64 = sqlx::query_scalar(
            "SELECT quantity::double precision FROM colo_asset_balance WHERE account_id = $1 AND asset = $2",
        )
        .bind(identity.account_id).bind(&quote).fetch_one(&pool).await.unwrap();
        assert!((quote_balance - 900.0).abs() < 1e-9);
        let fee_balance: f64 = sqlx::query_scalar(
            "SELECT quantity::double precision FROM colo_asset_balance WHERE account_id = $1 AND asset = 'BNB'",
        )
        .bind(identity.account_id).fetch_one(&pool).await.unwrap();
        assert!((fee_balance + 0.1).abs() < 1e-9);
    }

}
