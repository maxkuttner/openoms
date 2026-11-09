//! `/admin/strategies*`: the registry's admin surface. Every write reloads the
//! in-memory `RegistrySnapshot`.

use std::collections::BTreeSet;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use crate::admin::{map_db_error, AdminError};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use super::control::ControlReport;

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct StrategyRow {
    pub code: String,
    pub name: String,
    pub portfolio_id: Uuid,
    pub owner_principal_id: Uuid,
    pub principal_id: Uuid,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub has_fix_password: bool,
    /// Comma-separated IP addresses; empty means any.
    pub allowed_addresses: String,
    pub state: String,
    pub rest_enabled: bool,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>,
    pub last_logon_error: Option<String>,
    pub mode: String,
    pub follow_environment: Option<String>,
    pub has_follow_credentials: bool,
    pub dead_man_secs: i32,
    pub max_orders_per_sec: Option<i32>,
    pub kill_when_unmonitored: bool,
    pub limits_version: i64,
    /// Whether its FIX session is logged on right now.
    #[sqlx(default)]
    pub session_live: bool,
    /// Co-located strategies: the control link and the followed account.
    #[sqlx(default)]
    pub link_connected: bool,
    #[sqlx(default)]
    pub last_heard_secs: Option<i64>,
    #[sqlx(default)]
    pub acked_limits_version: i64,
    #[sqlx(default)]
    pub open_orders: i64,
    #[sqlx(default)]
    pub placed_per_min: i64,
    #[sqlx(default)]
    pub cancelled_per_min: i64,
    #[sqlx(default)]
    pub alarm: Option<String>,
    #[sqlx(default)]
    pub fighting: bool,
    /// Killed, and the strategy has not acknowledged it within 5 s.
    #[sqlx(default)]
    pub kill_ack_late: bool,
}

const ROW_SELECT: &str = "code, name, portfolio_id, owner_principal_id, principal_id, sender_comp_id, \
    target_comp_id, fix_password_hash IS NOT NULL AS has_fix_password, \
    array_to_string(allowed_addresses, ', ') AS allowed_addresses, state, rest_enabled, \
    max_order_quantity::double precision AS max_order_quantity, \
    max_order_notional::double precision AS max_order_notional, \
    max_position_quantity::double precision AS max_position_quantity, \
    max_position_notional::double precision AS max_position_notional, \
    price_band_pct::double precision AS price_band_pct, last_logon_error, mode, follow_environment, \
    follow_credentials IS NOT NULL AS has_follow_credentials, dead_man_secs, max_orders_per_sec, \
    kill_when_unmonitored, limits_version";

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateStrategy {
    pub code: String,
    pub name: String,
    pub portfolio_id: Uuid,
    pub owner_principal_id: Uuid,
    pub principal_id: Uuid,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    /// Comma-separated IP addresses; empty means any.
    pub allowed_addresses: Option<String>,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>,
    /// `VIA_OMS` (default) or `COLOCATED`.
    pub mode: Option<String>,
    pub follow_environment: Option<String>,
    pub dead_man_secs: Option<i32>,
    pub max_orders_per_sec: Option<i32>,
    pub kill_when_unmonitored: Option<bool>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateStrategy {
    pub name: Option<String>,
    pub sender_comp_id: Option<String>,
    pub target_comp_id: Option<String>,
    /// Comma-separated IP addresses; empty means any.
    pub allowed_addresses: Option<String>,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>,
    /// `VIA_OMS` (default) or `COLOCATED`.
    pub mode: Option<String>,
    pub follow_environment: Option<String>,
    pub dead_man_secs: Option<i32>,
    pub max_orders_per_sec: Option<i32>,
    pub kill_when_unmonitored: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ConnectionEntry {
    pub broker_code: String,
    pub account_id: Uuid,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct NewPassword {
    /// Shown once; only its hash is stored.
    pub password: String,
}

async fn reload(state: &AppState) {
    if let Err(e) = state.strategies().reload(state.pool()).await {
        warn!(error = %e, "strategies: registry reload failed; the snapshot keeps its last state");
    }
    crate::colo::follow::sync_follows(state).await;
}

fn bad_request(message: impl Into<String>) -> AdminError {
    AdminError::new(StatusCode::BAD_REQUEST, message)
}

/// Whether an update changes a limit the strategy enforces (so its version moves).
fn changes_limits(p: &UpdateStrategy) -> bool {
    p.max_order_quantity.is_some()
        || p.max_order_notional.is_some()
        || p.max_position_quantity.is_some()
        || p.max_position_notional.is_some()
        || p.price_band_pct.is_some()
        || p.max_orders_per_sec.is_some()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct FollowCredentials {
    /// The OMS's own key on the strategy's venue account (read + cancel).
    pub api_key: String,
    /// Its Ed25519 private key (PEM).
    pub private_key: String,
}

/// The sealing context of a strategy's follow credential.
pub(crate) fn follow_aad(code: &str) -> String {
    format!("strategy:{code}:follow")
}

/// Store the OMS's follow-only key for a co-located strategy, sealed with the
/// master key. It is never returned.
#[utoipa::path(
    put, path = "/admin/strategies/{code}/follow-credentials", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    request_body = FollowCredentials,
    responses((status = 204, description = "Stored"), (status = 404, description = "Not found"), (status = 503, description = "No master key")),
    security(("bearer_token" = []))
)]
pub async fn put_follow_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(p): Json<FollowCredentials>,
) -> Result<StatusCode, AdminError> {
    info!(%code, "admin store strategy follow credentials");
    let key = crate::admin::resolve_master_key()?
        .ok_or_else(|| AdminError::new(StatusCode::SERVICE_UNAVAILABLE, "no master key is configured; cannot store credentials"))?;
    let plain = serde_json::to_vec(&serde_json::json!({ "api_key": p.api_key, "private_key": p.private_key }))
        .map_err(|e| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let sealed = crate::secrets::seal(&key, &follow_aad(&code), &plain);
    let done = sqlx::query(
        "UPDATE strategy SET follow_credentials = $2, follow_since = COALESCE(follow_since, now()), updated_at = now() WHERE code = $1",
    )
        .bind(&code)
        .bind(sealed)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;
    if done.rows_affected() == 0 {
        return Err(AdminError::not_found("strategy"));
    }
    reload(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// "1.2.3.4, 5.6.7.8" → the addresses, blanks dropped.
fn addresses(list: &str) -> Vec<String> {
    list.split(',').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect()
}

/// Empty CompIDs are a caller error.
fn validate_comp_ids(sender: &str, target: &str) -> Result<(), AdminError> {
    if sender.trim().is_empty() || target.trim().is_empty() {
        return Err(bad_request("sender_comp_id and target_comp_id are required"));
    }
    Ok(())
}

#[utoipa::path(
    get, path = "/admin/strategies", tag = "admin",
    responses((status = 200, description = "Every strategy", body = [StrategyRow])),
    security(("bearer_token" = []))
)]
pub async fn list_strategies(State(state): State<AppState>) -> Result<Json<Vec<StrategyRow>>, AdminError> {
    let mut rows = sqlx::query_as::<_, StrategyRow>(&format!("SELECT {ROW_SELECT} FROM strategy ORDER BY code"))
        .fetch_all(state.pool())
        .await
        .map_err(map_db_error)?;
    // v2 deployments report kill stages to the database, not the hub.
    let stalled = match crate::colo::deployment::stalled_kills(state.pool()).await {
        Ok(stalled) => stalled,
        Err(e) => {
            tracing::warn!(error = %e, "strategies: could not check deployment kill acknowledgements");
            Default::default()
        }
    };
    for row in &mut rows {
        row.session_live = state.fix_sessions().is_live(&row.code);
        if row.mode == "COLOCATED" {
            let hub = state.colo();
            let link = hub.link_status(&row.code);
            let flow = hub.flow(&row.code);
            row.link_connected = link.connected;
            row.last_heard_secs = link.last_heard_secs.map(|s| s as i64);
            row.acked_limits_version = link.acked_version;
            row.open_orders = hub.open_orders(&row.code) as i64;
            row.placed_per_min = flow.placed as i64;
            row.cancelled_per_min = flow.cancelled as i64;
            row.alarm = hub.alarm(&row.code);
            row.fighting = hub.fighting(&row.code);
            row.kill_ack_late =
                (row.state == "HALTED" && hub.kill_ack_late(&row.code)) || stalled.contains(&row.code);
        }
    }
    Ok(Json(rows))
}

#[utoipa::path(
    post, path = "/admin/strategies", tag = "admin",
    request_body = CreateStrategy,
    responses(
        (status = 200, description = "Created", body = StrategyRow),
        (status = 400, description = "Missing CompIDs"),
        (status = 409, description = "Code, portfolio, principal or CompIDs already used"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_strategy(
    State(state): State<AppState>,
    Json(p): Json<CreateStrategy>,
) -> Result<Json<StrategyRow>, AdminError> {
    validate_comp_ids(&p.sender_comp_id, &p.target_comp_id)?;
    info!(code = %p.code, "admin create strategy");
    let mut tx = state.pool().begin().await.map_err(map_db_error)?;
    let row = sqlx::query_as::<_, StrategyRow>(&format!(
        "INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, principal_id, sender_comp_id, \
            target_comp_id, allowed_addresses, max_order_quantity, max_order_notional, max_position_quantity, \
            max_position_notional, price_band_pct, mode, follow_environment, dead_man_secs, max_orders_per_sec, \
            kill_when_unmonitored) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, COALESCE($14, 'VIA_OMS'), $15, \
            COALESCE($16, 5), $17, COALESCE($18, false)) RETURNING {ROW_SELECT}"
    ))
    .bind(&p.code)
    .bind(&p.name)
    .bind(p.portfolio_id)
    .bind(p.owner_principal_id)
    .bind(p.principal_id)
    .bind(&p.sender_comp_id)
    .bind(&p.target_comp_id)
    .bind(addresses(p.allowed_addresses.as_deref().unwrap_or_default()))
    .bind(p.max_order_quantity)
    .bind(p.max_order_notional)
    .bind(p.max_position_quantity)
    .bind(p.max_position_notional)
    .bind(p.price_band_pct)
    .bind(p.mode)
    .bind(p.follow_environment)
    .bind(p.dead_man_secs)
    .bind(p.max_orders_per_sec)
    .bind(p.kill_when_unmonitored)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_db_error)?;
    // The strategy trades its portfolio as its own principal, so it needs the
    // trade grant every order path checks.
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
         VALUES ($1, $2, $3, true, true, false) \
         ON CONFLICT (principal_id, portfolio_id) DO UPDATE SET can_trade = true, can_view = true",
    )
    .bind(Uuid::new_v4())
    .bind(p.principal_id)
    .bind(p.portfolio_id)
    .execute(&mut *tx)
    .await
    .map_err(map_db_error)?;
    tx.commit().await.map_err(map_db_error)?;
    reload(&state).await;
    Ok(Json(row))
}

#[utoipa::path(
    patch, path = "/admin/strategies/{code}", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    request_body = UpdateStrategy,
    responses((status = 200, description = "Updated", body = StrategyRow), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn update_strategy(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(p): Json<UpdateStrategy>,
) -> Result<Json<StrategyRow>, AdminError> {
    info!(%code, "admin update strategy");
    let bump = changes_limits(&p);
    let row = sqlx::query_as::<_, StrategyRow>(&format!(
        "UPDATE strategy SET \
            name = COALESCE($2, name), \
            sender_comp_id = COALESCE($3, sender_comp_id), \
            target_comp_id = COALESCE($4, target_comp_id), \
            allowed_addresses = COALESCE($5, allowed_addresses), \
            max_order_quantity = COALESCE($6, max_order_quantity), \
            max_order_notional = COALESCE($7, max_order_notional), \
            max_position_quantity = COALESCE($8, max_position_quantity), \
            max_position_notional = COALESCE($9, max_position_notional), \
            price_band_pct = COALESCE($10, price_band_pct), \
            mode = COALESCE($11, mode), \
            follow_environment = COALESCE($12, follow_environment), \
            dead_man_secs = COALESCE($13, dead_man_secs), \
            max_orders_per_sec = COALESCE($14, max_orders_per_sec), \
            kill_when_unmonitored = COALESCE($15, kill_when_unmonitored), \
            limits_version = limits_version + CASE WHEN $16 THEN 1 ELSE 0 END, \
            updated_at = now() \
         WHERE code = $1 RETURNING {ROW_SELECT}"
    ))
    .bind(&code)
    .bind(p.name)
    .bind(p.sender_comp_id)
    .bind(p.target_comp_id)
    .bind(p.allowed_addresses.as_deref().map(addresses))
    .bind(p.max_order_quantity)
    .bind(p.max_order_notional)
    .bind(p.max_position_quantity)
    .bind(p.max_position_notional)
    .bind(p.price_band_pct)
    .bind(p.mode)
    .bind(p.follow_environment)
    .bind(p.dead_man_secs)
    .bind(p.max_orders_per_sec)
    .bind(p.kill_when_unmonitored)
    .bind(bump)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("strategy"))?;
    validate_comp_ids(&row.sender_comp_id, &row.target_comp_id)?;
    reload(&state).await;
    if row.mode == "COLOCATED" {
        if bump {
            if let Some(s) = state.strategies().by_code(&code) {
                state.colo().push(&code, colo_link::proto::ToStrategy::Limits { limits: s.risk_limits(), limits_version: s.limits_version.max(0) as u64 });
            }
        }
        if let Err(e) = crate::colo::deployment::publish_policy(&state, &code).await {
            warn!(%code, error = %e, "strategies: could not publish the v2 policy snapshot");
        }
    }
    Ok(Json(row))
}

#[utoipa::path(
    delete, path = "/admin/strategies/{code}", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn delete_strategy(State(state): State<AppState>, Path(code): Path<String>) -> Result<StatusCode, AdminError> {
    info!(%code, "admin delete strategy");
    let done = sqlx::query("DELETE FROM strategy WHERE code = $1")
        .bind(&code)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;
    if done.rows_affected() == 0 {
        return Err(AdminError::not_found("strategy"));
    }
    reload(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The strategy's connections: one account per broker code.
#[utoipa::path(
    get, path = "/admin/strategies/{code}/connections", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 200, description = "Its connections", body = [ConnectionEntry])),
    security(("bearer_token" = []))
)]
pub async fn get_connections(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<Vec<ConnectionEntry>>, AdminError> {
    let rows = sqlx::query_as::<_, ConnectionEntry>(
        "SELECT broker_code, account_id FROM strategy_connection WHERE strategy_code = $1 ORDER BY broker_code",
    )
    .bind(&code)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(rows))
}

/// Replace the strategy's connections. Each account must belong to the
/// strategy's portfolio and sit on a connection of the named broker; a broker
/// code may appear once.
#[utoipa::path(
    put, path = "/admin/strategies/{code}/connections", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    request_body = [ConnectionEntry],
    responses(
        (status = 204, description = "Replaced"),
        (status = 400, description = "A broker twice, or an account of another portfolio or broker"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn put_connections(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(entries): Json<Vec<ConnectionEntry>>,
) -> Result<StatusCode, AdminError> {
    let mut seen = BTreeSet::new();
    for e in &entries {
        if !seen.insert(e.broker_code.as_str()) {
            return Err(bad_request(format!("broker {} is listed twice; a strategy has one connection per broker", e.broker_code)));
        }
    }
    let mut tx = state.pool().begin().await.map_err(map_db_error)?;
    let portfolio_id: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1 FOR UPDATE")
        .bind(&code)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| AdminError::not_found("strategy"))?;
    for e in &entries {
        let found = sqlx::query_as::<_, (Option<Uuid>, String)>(
            "SELECT a.portfolio_id, bc.broker_code FROM account a \
             JOIN broker_connection bc ON bc.code = a.broker_connection_code WHERE a.id = $1",
        )
        .bind(e.account_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| bad_request(format!("account {} not found", e.account_id)))?;
        check_connection(portfolio_id, e, found)?;
    }
    sqlx::query("DELETE FROM strategy_connection WHERE strategy_code = $1")
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
    for e in &entries {
        sqlx::query("INSERT INTO strategy_connection (strategy_code, broker_code, account_id) VALUES ($1, $2, $3)")
            .bind(&code)
            .bind(&e.broker_code)
            .bind(e.account_id)
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
    }
    tx.commit().await.map_err(map_db_error)?;
    reload(&state).await;
    if let Err(e) = crate::colo::deployment::publish_policy(&state, &code).await {
        warn!(%code, error = %e, "strategies: could not publish the v2 policy snapshot");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `found` is the account's (portfolio_id, broker_code).
fn check_connection(portfolio_id: Uuid, e: &ConnectionEntry, found: (Option<Uuid>, String)) -> Result<(), AdminError> {
    if found.0 != Some(portfolio_id) {
        return Err(bad_request(format!("account {} does not belong to the strategy's portfolio", e.account_id)));
    }
    if found.1 != e.broker_code {
        return Err(bad_request(format!("account {} is on broker {}, not {}", e.account_id, found.1, e.broker_code)));
    }
    Ok(())
}

/// Set a new FIX password and return it once.
#[utoipa::path(
    post, path = "/admin/strategies/{code}/password", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 200, description = "The new password, shown once", body = NewPassword), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn reset_password(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<NewPassword>, AdminError> {
    info!(%code, "admin reset strategy FIX password");
    let password = super::new_fix_password();
    let to_hash = password.clone();
    let hash = tokio::task::spawn_blocking(move || super::hash_fix_password(&to_hash))
        .await
        .map_err(|_| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "hash task failed"))?;
    let done = sqlx::query("UPDATE strategy SET fix_password_hash = $2, updated_at = now() WHERE code = $1")
        .bind(&code)
        .bind(hash)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;
    if done.rows_affected() == 0 {
        return Err(AdminError::not_found("strategy"));
    }
    reload(&state).await;
    Ok(Json(NewPassword { password }))
}

fn control_error(e: crate::handlers::ApiError) -> AdminError {
    AdminError::new(e.status, e.message)
}

/// Halt the strategy, refuse its REST token, cancel every open order and log
/// its FIX session out. Positions are left as they are.
#[utoipa::path(
    post, path = "/admin/strategies/{code}/kill", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 200, description = "Killed", body = ControlReport), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn kill_strategy(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<ControlReport>, AdminError> {
    info!(%code, "admin kill strategy");
    super::control::kill(&state, &code).await.map(Json).map_err(control_error)
}

/// Let a killed or flattened strategy trade and log on again.
#[utoipa::path(
    post, path = "/admin/strategies/{code}/enable", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 204, description = "Enabled"), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn enable_strategy(State(state): State<AppState>, Path(code): Path<String>) -> Result<StatusCode, AdminError> {
    info!(%code, "admin enable strategy");
    super::control::enable(&state, &code).await.map(|_| StatusCode::NO_CONTENT).map_err(control_error)
}

/// Kill, leave the strategy reducing only, and close every position with a
/// limit order inside its price band.
#[utoipa::path(
    post, path = "/admin/strategies/{code}/flatten", tag = "admin",
    params(("code" = String, Path, description = "Strategy code")),
    responses((status = 200, description = "Flattening", body = ControlReport), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn flatten_strategy(State(state): State<AppState>, Path(code): Path<String>) -> Result<Json<ControlReport>, AdminError> {
    info!(%code, "admin flatten strategy");
    super::control::flatten(&state, &code).await.map(Json).map_err(control_error)
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AcceptorRestarted {
    /// Strategy sessions that were logged on and were logged out by the restart.
    pub logged_out: usize,
}

/// Restart the FIX acceptor with a session per strategy as registered now.
#[utoipa::path(
    post, path = "/admin/fix-acceptor/restart", tag = "admin",
    responses(
        (status = 200, description = "Restarted", body = AcceptorRestarted),
        (status = 500, description = "The acceptor did not start again"),
    ),
    security(("bearer_token" = []))
)]
pub async fn restart_acceptor(State(state): State<AppState>) -> Result<Json<AcceptorRestarted>, AdminError> {
    info!("admin restart FIX acceptor");
    let s = state.clone();
    let logged_out = tokio::task::spawn_blocking(move || s.fix_sessions().restart(&s))
        .await
        .map_err(|_| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "restart task failed"))?
        .map_err(|e| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("FIX acceptor did not start: {e}")))?;
    Ok(Json(AcceptorRestarted { logged_out }))
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TradeableInstrument {
    /// The broker code an order names as its `venue` (FIX tag 100).
    pub venue: String,
    /// That broker's symbol (FIX tag 55).
    pub symbol: String,
    pub instrument_id: i64,
}

/// What the calling strategy may trade.
#[utoipa::path(
    get, path = "/strategies/securities", tag = "orders",
    responses(
        (status = 200, description = "Tradeable instruments", body = [TradeableInstrument]),
        (status = 404, description = "The caller is not a strategy"),
    ),
    security(("bearer_token" = []))
)]
pub async fn strategy_securities(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<TradeableInstrument>>, AdminError> {
    let strategy = state
        .strategies()
        .all()
        .iter()
        .find(|s| s.principal_id == auth.principal_id)
        .cloned()
        .ok_or_else(|| AdminError::not_found("strategy"))?;
    let rows = super::session::tradeable(state.pool(), &strategy).await.map_err(map_db_error)?;
    Ok(Json(rows.into_iter().map(|(venue, symbol, instrument_id)| TradeableInstrument { venue, symbol, instrument_id }).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(broker: &str) -> ConnectionEntry {
        ConnectionEntry { broker_code: broker.into(), account_id: Uuid::nil() }
    }

    #[test]
    fn a_limit_change_bumps_the_limits_version() {
        let none: UpdateStrategy = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        let limit: UpdateStrategy = serde_json::from_str(r#"{"max_order_notional":5}"#).unwrap();
        let rate: UpdateStrategy = serde_json::from_str(r#"{"max_orders_per_sec":10}"#).unwrap();
        assert!(!changes_limits(&none));
        assert!(changes_limits(&limit));
        assert!(changes_limits(&rate));
    }

    #[test]
    fn allowed_addresses_are_comma_separated() {
        assert_eq!(addresses(" 10.0.4.21, ,10.0.4.22 "), vec!["10.0.4.21".to_string(), "10.0.4.22".to_string()]);
        assert!(addresses("").is_empty());
    }

    #[test]
    fn a_connection_must_be_the_strategys_portfolio_and_broker() {
        let p = Uuid::new_v4();
        assert!(check_connection(p, &entry("BINANCE"), (Some(p), "BINANCE".into())).is_ok());
        assert!(check_connection(p, &entry("BINANCE"), (Some(Uuid::new_v4()), "BINANCE".into())).is_err());
        assert!(check_connection(p, &entry("BINANCE"), (None, "BINANCE".into())).is_err());
        assert!(check_connection(p, &entry("BINANCE"), (Some(p), "BYBIT".into())).is_err());
    }

    /// Against the shared dev DB: a reset returns the plaintext once and stores
    /// only a hash that verifies it.
    #[tokio::test]
    #[ignore]
    async fn password_reset_returns_plaintext_once_and_stores_a_hash() {
        let pool = crate::strategies::test_support::dev_pool().await;
        let code = crate::strategies::test_support::seed_strategy(&pool).await;
        let state = crate::strategies::test_support::state(pool.clone());
        let Json(NewPassword { password }) = reset_password(State(state), Path(code.clone())).await.unwrap();
        let hash: String = sqlx::query_scalar("SELECT fix_password_hash FROM strategy WHERE code = $1")
            .bind(&code).fetch_one(&pool).await.unwrap();
        assert!(!hash.contains(&password));
        assert!(super::super::verify_fix_password(&password, &hash));
        crate::strategies::test_support::drop_strategy(&pool, &code).await;
    }
}
