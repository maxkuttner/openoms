//! Admin API for external-order reconciliation: a connection's recon settings,
//! its attribution rules, an on-demand sweep, and the breaks a person resolves.
//!
//! Attribution rules and colo: registering a co-located deployment replaces the
//! account's prefix-less rules (`colo::deployment`), so an account-only rule
//! made here for a colo account is overwritten at the next registration.
//! Deleting such a rule is allowed; the account then falls back to the
//! connection's default portfolio until the next registration.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::admin::{map_db_error, AdminError};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::ingest::apply::{event_from_json, ingest_with_portfolio, IngestError};
use crate::ingest::decide::Decision;
use crate::jobs::eod_sweep::{run_connection, Lookback};
use crate::reconciler::SweepReport;

type ApiResult<T> = Result<Json<T>, AdminError>;

fn bad_request(message: impl Into<String>) -> AdminError {
    AdminError::new(StatusCode::BAD_REQUEST, message)
}

fn conflict(message: impl Into<String>) -> AdminError {
    AdminError::new(StatusCode::CONFLICT, message)
}

async fn require_connection(pool: &PgPool, code: &str) -> Result<(), AdminError> {
    let found: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM broker_connection WHERE code = $1)")
        .bind(code)
        .fetch_one(pool)
        .await
        .map_err(map_db_error)?;
    found.then_some(()).ok_or_else(|| AdminError::not_found("broker_connection"))
}

async fn require_portfolio(pool: &PgPool, id: Uuid) -> Result<(), AdminError> {
    let found: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM portfolio WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(map_db_error)?;
    found.then_some(()).ok_or_else(|| AdminError::not_found("portfolio"))
}

/// The acting principal, for `resolved_by`; the host token has none.
fn principal_of(auth: &AuthContext) -> Option<Uuid> {
    (!auth.principal_id.is_nil()).then_some(auth.principal_id)
}

// ---- recon settings ----

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReconSettings {
    pub code: String,
    pub default_portfolio_id: Option<Uuid>,
    pub external_policy: String,
    pub eod_lookback: String,
    pub intraday_poll_secs: Option<i32>,
}

/// Fields left out stay as they are (none of them can be cleared).
#[derive(Debug, Default, Deserialize)]
pub struct ReconPatch {
    pub default_portfolio_id: Option<Uuid>,
    /// `adopt` | `break`
    pub external_policy: Option<String>,
    /// `1d` | `1w` | `1m` | `1y` | `5y` | `10y`
    pub eod_lookback: Option<String>,
    pub intraday_poll_secs: Option<i32>,
}

fn parse_lookback(text: &str) -> Result<Lookback, AdminError> {
    text.parse::<Lookback>().map_err(bad_request)
}

pub async fn get_recon(State(state): State<AppState>, Path(code): Path<String>) -> ApiResult<ReconSettings> {
    sqlx::query_as::<_, ReconSettings>(
        "SELECT code, default_portfolio_id, external_policy, eod_lookback, intraday_poll_secs FROM broker_connection WHERE code = $1",
    )
    .bind(&code)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .map(Json)
    .ok_or_else(|| AdminError::not_found("broker_connection"))
}

pub async fn patch_recon(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(patch): Json<ReconPatch>,
) -> ApiResult<ReconSettings> {
    if let Some(policy) = &patch.external_policy {
        if !matches!(policy.as_str(), "adopt" | "break") {
            return Err(bad_request("external_policy must be adopt or break"));
        }
    }
    if let Some(lookback) = &patch.eod_lookback {
        parse_lookback(lookback)?;
    }
    if patch.intraday_poll_secs.is_some_and(|s| s < 1) {
        return Err(bad_request("intraday_poll_secs must be at least 1"));
    }
    require_connection(state.pool(), &code).await?;
    if let Some(portfolio) = patch.default_portfolio_id {
        require_portfolio(state.pool(), portfolio).await?;
    }
    let settings = sqlx::query_as::<_, ReconSettings>(
        "UPDATE broker_connection SET \
            default_portfolio_id = COALESCE($2, default_portfolio_id), \
            external_policy = COALESCE($3, external_policy), \
            eod_lookback = COALESCE($4, eod_lookback), \
            intraday_poll_secs = COALESCE($5, intraday_poll_secs), \
            updated_at = now() \
         WHERE code = $1 \
         RETURNING code, default_portfolio_id, external_policy, eod_lookback, intraday_poll_secs",
    )
    .bind(&code)
    .bind(patch.default_portfolio_id)
    .bind(patch.external_policy)
    .bind(patch.eod_lookback)
    .bind(patch.intraday_poll_secs)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(settings))
}

// ---- attribution rules ----

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AttributionRule {
    pub id: Uuid,
    pub broker_connection_code: String,
    pub account_id: Option<Uuid>,
    pub client_id_prefix: Option<String>,
    pub portfolio_id: Uuid,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct RuleBody {
    pub account_id: Option<Uuid>,
    pub client_id_prefix: Option<String>,
    pub portfolio_id: Uuid,
    /// Lowest wins.
    pub priority: i32,
}

const RULE_COLUMNS: &str = "id, broker_connection_code, account_id, client_id_prefix, portfolio_id, priority, created_at";

/// A rule needs an account or a prefix; its portfolio and account must exist,
/// the account on this connection.
async fn check_rule(pool: &PgPool, code: &str, body: &RuleBody) -> Result<(), AdminError> {
    require_connection(pool, code).await?;
    if body.account_id.is_none() && body.client_id_prefix.as_deref().map_or(true, str::is_empty) {
        return Err(bad_request("a rule needs an account_id or a client_id_prefix"));
    }
    require_portfolio(pool, body.portfolio_id).await?;
    if let Some(account) = body.account_id {
        let of: Option<String> = sqlx::query_scalar("SELECT broker_connection_code FROM account WHERE id = $1")
            .bind(account)
            .fetch_optional(pool)
            .await
            .map_err(map_db_error)?;
        match of {
            None => return Err(AdminError::not_found("account")),
            Some(c) if c != code => return Err(bad_request("the account belongs to another connection")),
            Some(_) => {}
        }
    }
    Ok(())
}

pub async fn list_rules(State(state): State<AppState>, Path(code): Path<String>) -> ApiResult<Vec<AttributionRule>> {
    require_connection(state.pool(), &code).await?;
    let rules = sqlx::query_as::<_, AttributionRule>(&format!(
        "SELECT {RULE_COLUMNS} FROM attribution_rule WHERE broker_connection_code = $1 ORDER BY priority, created_at"
    ))
    .bind(&code)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(rules))
}

pub async fn create_rule(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(body): Json<RuleBody>,
) -> Result<(StatusCode, Json<AttributionRule>), AdminError> {
    check_rule(state.pool(), &code, &body).await?;
    let rule = sqlx::query_as::<_, AttributionRule>(&format!(
        "INSERT INTO attribution_rule (id, broker_connection_code, account_id, client_id_prefix, portfolio_id, priority) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {RULE_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(&code)
    .bind(body.account_id)
    .bind(body.client_id_prefix.filter(|p| !p.is_empty()))
    .bind(body.portfolio_id)
    .bind(body.priority)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok((StatusCode::CREATED, Json(rule)))
}

pub async fn update_rule(
    State(state): State<AppState>,
    Path((code, id)): Path<(String, Uuid)>,
    Json(body): Json<RuleBody>,
) -> ApiResult<AttributionRule> {
    check_rule(state.pool(), &code, &body).await?;
    let rule = sqlx::query_as::<_, AttributionRule>(&format!(
        "UPDATE attribution_rule SET account_id = $3, client_id_prefix = $4, portfolio_id = $5, priority = $6 \
         WHERE id = $1 AND broker_connection_code = $2 RETURNING {RULE_COLUMNS}"
    ))
    .bind(id)
    .bind(&code)
    .bind(body.account_id)
    .bind(body.client_id_prefix.filter(|p| !p.is_empty()))
    .bind(body.portfolio_id)
    .bind(body.priority)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("attribution_rule"))?;
    Ok(Json(rule))
}

pub async fn delete_rule(State(state): State<AppState>, Path((code, id)): Path<(String, Uuid)>) -> Result<StatusCode, AdminError> {
    let deleted = sqlx::query("DELETE FROM attribution_rule WHERE id = $1 AND broker_connection_code = $2")
        .bind(id)
        .bind(&code)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?
        .rows_affected();
    if deleted == 0 {
        return Err(AdminError::not_found("attribution_rule"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- on-demand sweep ----

#[derive(Debug, Default, Deserialize)]
pub struct SweepBody {
    /// Defaults to the connection's `eod_lookback`.
    pub lookback: Option<String>,
}

/// Sweep the connection now, as the nightly job does.
pub async fn sweep(
    State(state): State<AppState>,
    Path(code): Path<String>,
    body: Result<Json<SweepBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<SweepReport> {
    use axum::extract::rejection::JsonRejection;
    let requested = match body {
        Ok(Json(b)) => b.lookback,
        // no body at all means the defaults; a body that is there must be valid
        Err(JsonRejection::MissingJsonContentType(_)) => None,
        Err(e) => return Err(bad_request(format!("invalid body: {e}"))),
    };
    let requested = requested.as_deref().map(parse_lookback).transpose()?;
    let configured: Option<String> = sqlx::query_scalar("SELECT eod_lookback FROM broker_connection WHERE code = $1")
        .bind(&code)
        .fetch_optional(state.pool())
        .await
        .map_err(map_db_error)?;
    let configured = configured.ok_or_else(|| AdminError::not_found("broker_connection"))?;
    let lookback = match requested {
        Some(l) => l,
        None => parse_lookback(&configured)?,
    };
    run_connection(state.pool(), &code, lookback)
        .await
        .map(Json)
        .ok_or_else(|| conflict("the connection has no live reconciler"))
}

// ---- breaks ----

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReconBreak {
    pub id: Uuid,
    /// `RECONCILED` (a venue order) or `REPORTED` (a reported fill).
    pub source: String,
    /// Absent for REPORTED breaks.
    pub broker_connection_code: Option<String>,
    pub account_id: Option<Uuid>,
    pub venue_order_id: Option<String>,
    /// REPORTED breaks only.
    pub strategy_code: Option<String>,
    /// The raw reported fill, as received (REPORTED breaks only).
    #[sqlx(rename = "reported_fill_json")]
    pub reported_fill: Option<serde_json::Value>,
    pub kind: String,
    /// The venue events held, in arrival order.
    #[sqlx(rename = "events_json")]
    pub events: serde_json::Value,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolved_by: Option<Uuid>,
}

const BREAK_COLUMNS: &str =
    "id, source, broker_connection_code, account_id, venue_order_id, strategy_code, reported_fill_json, \
     kind, events_json, status, created_at, resolved_at, resolved_by";

#[derive(Debug, Default, Deserialize)]
pub struct BreakFilter {
    /// `open` | `resolved` | `dismissed`; all when left out.
    pub status: Option<String>,
    /// `RECONCILED` | `REPORTED`; both when left out.
    pub source: Option<String>,
}

pub async fn list_breaks(State(state): State<AppState>, Query(filter): Query<BreakFilter>) -> ApiResult<Vec<ReconBreak>> {
    if filter.status.as_deref().is_some_and(|s| !matches!(s, "open" | "resolved" | "dismissed")) {
        return Err(bad_request("status must be open, resolved or dismissed"));
    }
    if filter.source.as_deref().is_some_and(|s| !matches!(s, "RECONCILED" | "REPORTED")) {
        return Err(bad_request("source must be RECONCILED or REPORTED"));
    }
    let breaks = sqlx::query_as::<_, ReconBreak>(&format!(
        "SELECT {BREAK_COLUMNS} FROM recon_break WHERE ($1::text IS NULL OR status = $1) AND ($2::text IS NULL OR source = $2) \
         ORDER BY created_at DESC LIMIT 500"
    ))
    .bind(filter.status)
    .bind(filter.source)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(breaks))
}

async fn open_break(pool: &PgPool, id: Uuid) -> Result<ReconBreak, AdminError> {
    let found = sqlx::query_as::<_, ReconBreak>(&format!("SELECT {BREAK_COLUMNS} FROM recon_break WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| AdminError::not_found("recon_break"))?;
    if found.status != "open" {
        return Err(conflict(format!("the break is already {}", found.status)));
    }
    Ok(found)
}

/// Close an open break as `status`; a concurrent resolution loses with 409.
/// `None` when it is no longer open, or (with `events`) when it holds other
/// than that many events: one arrived since the caller read it.
async fn close_break(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    by: Option<Uuid>,
    events: Option<i32>,
) -> Result<Option<ReconBreak>, AdminError> {
    sqlx::query_as::<_, ReconBreak>(&format!(
        "UPDATE recon_break SET status = $2, resolved_at = now(), resolved_by = $3 \
         WHERE id = $1 AND status = 'open' AND ($4::int IS NULL OR jsonb_array_length(events_json) = $4) \
         RETURNING {BREAK_COLUMNS}"
    ))
    .bind(id)
    .bind(status)
    .bind(by)
    .bind(events)
    .fetch_optional(pool)
    .await
    .map_err(map_db_error)
}

/// How often an adopt re-reads a break that keeps collecting events.
const ADOPT_ROUNDS: usize = 5;

/// RECONCILED breaks need `portfolio_id`. REPORTED breaks need `instrument_id`
/// (an instrument id; a JSON string or number); `portfolio_id` defaults to the
/// strategy's own portfolio (another one is an explicit, audited rebook), and
/// `map_symbol` (default true) maps the fill's venue symbol to the instrument so
/// later fills book.
#[derive(Debug, Default, Deserialize)]
pub struct AdoptBody {
    pub portfolio_id: Option<Uuid>,
    #[serde(default, deserialize_with = "id_any")]
    pub instrument_id: Option<String>,
    pub map_symbol: Option<bool>,
}

fn id_any<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(serde::de::Error::custom("instrument_id must be a string or number")),
    }
}

/// Book the break's events into `portfolio_id` and resolve it. Each event books
/// in its own transaction (fills dedupe on their exec id), so when one cannot
/// book, the earlier ones stay booked, the break stays open and the call can be
/// repeated once the cause is fixed.
///
/// The stream keeps appending to an open break, so the break closes only if it
/// still holds exactly the events that were booked; otherwise the new ones are
/// read and booked too (up to `ADOPT_ROUNDS` times, then 409).
pub async fn adopt_break(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<AdoptBody>,
) -> ApiResult<ReconBreak> {
    let found = open_break(state.pool(), id).await?;
    if found.source == "REPORTED" {
        return adopt_reported(&state, &auth, found, body).await.map(Json);
    }
    let portfolio = body.portfolio_id.ok_or_else(|| bad_request("portfolio_id is required"))?;
    adopt(&state, &auth, id, portfolio, None).await.map(Json)
}

fn unprocessable(message: impl Into<String>) -> AdminError {
    AdminError::new(StatusCode::UNPROCESSABLE_ENTITY, message)
}

/// Book a REPORTED break's stored fill, exactly once, and resolve the break, all
/// in one transaction (the close is the lock: a concurrent adopt loses with 409).
/// A fill that cannot be read back (INVALID_FILL) cannot be adopted, and a
/// CONFLICTING_DUPLICATE contradicts a booked fill: both are 422; dismiss them.
async fn adopt_reported(state: &AppState, auth: &AuthContext, found: ReconBreak, body: AdoptBody) -> Result<ReconBreak, AdminError> {
    use crate::reported::ingest::{book_fill, Outcome, ReportedFill, CONFLICTING_DUPLICATE};
    if found.kind == CONFLICTING_DUPLICATE {
        return Err(unprocessable("the fill conflicts with a booked fill; dismiss it or correct it at source"));
    }
    let raw = found.reported_fill.clone().unwrap_or(serde_json::Value::Null);
    let fill = serde_json::from_value::<ReportedFill>(raw.clone())
        .map_err(|e| e.to_string())
        .and_then(|f| f.validate().map(|()| f))
        .map_err(|why| unprocessable(format!("the stored fill is not valid ({why}); adopting cannot invent missing fields, dismiss it")))?;
    let strategy = found.strategy_code.clone().ok_or_else(|| unprocessable("the break has no strategy"))?;
    let instrument: i64 = body
        .instrument_id
        .as_deref()
        .ok_or_else(|| bad_request("instrument_id is required"))?
        .trim()
        .parse()
        .map_err(|_| bad_request("instrument_id must be an instrument id"))?;
    let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM instrument WHERE id = $1)")
        .bind(instrument)
        .fetch_one(state.pool())
        .await
        .map_err(map_db_error)?;
    if !known {
        return Err(AdminError::not_found("instrument"));
    }
    let own: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
        .bind(&strategy)
        .fetch_optional(state.pool())
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| AdminError::not_found("strategy"))?;
    let portfolio = body.portfolio_id.unwrap_or(own);
    require_portfolio(state.pool(), portfolio).await?;

    let mut tx = state.pool().begin().await.map_err(map_db_error)?;
    let closed = sqlx::query_as::<_, ReconBreak>(&format!(
        "UPDATE recon_break SET status = 'resolved', resolved_at = now(), resolved_by = $2 \
         WHERE id = $1 AND status = 'open' RETURNING {BREAK_COLUMNS}"
    ))
    .bind(found.id)
    .bind(principal_of(auth))
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| conflict("the break was resolved by someone else"))?;
    if body.map_symbol.unwrap_or(true) {
        let mapped: Vec<i64> = sqlx::query_scalar("SELECT instrument_id FROM broker_instrument WHERE broker_code = $1 AND broker_symbol = $2")
            .bind(&fill.venue)
            .bind(&fill.symbol)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_db_error)?;
        if let Some(other) = mapped.iter().find(|m| **m != instrument) {
            return Err(conflict(format!(
                "{}/{} is already mapped to instrument {other}; pass map_symbol=false to book only this fill",
                fill.venue, fill.symbol
            )));
        }
        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol) \
             SELECT $1, $2, $3 WHERE NOT EXISTS (SELECT 1 FROM broker_instrument WHERE broker_code = $2 AND broker_symbol = $3)",
        )
        .bind(instrument)
        .bind(&fill.venue)
        .bind(&fill.symbol)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
    }
    // The fill row needs a batch; adoption gets its own, outside the sequence.
    let batch_id = format!("adopt-{}", found.id);
    sqlx::query(
        "INSERT INTO reported_batch (strategy_code, batch_id, booked, response_json, payload_json) \
         VALUES ($1, $2, 1, '{}', $3) ON CONFLICT DO NOTHING",
    )
    .bind(&strategy)
    .bind(&batch_id)
    .bind(&raw)
    .execute(&mut *tx)
    .await
    .map_err(map_db_error)?;
    let done = book_fill(&mut tx, &strategy, portfolio, instrument, &fill, &batch_id, &raw, (portfolio != own).then_some(portfolio)).await.map_err(map_db_error)?;
    match done.outcome {
        Outcome::Booked => {}
        Outcome::Duplicate => {} // booked meanwhile through the mapping: the break just closes
        Outcome::Break => return Err(unprocessable("the fill conflicts with a booked fill; dismiss it or correct it at source")),
    }
    if portfolio != own {
        sqlx::query("INSERT INTO audit_event (actor, action, detail) VALUES ($1, 'recon.reported_break_rebooked', $2)")
            .bind(auth.principal_code.as_str())
            .bind(serde_json::json!({
                "break_id": found.id, "strategy": strategy, "from_portfolio": own, "to_portfolio": portfolio,
                "exec_id": fill.exec_id, "instrument_id": instrument,
            }))
            .execute(&mut *tx)
            .await
            .map_err(map_db_error)?;
    }
    tx.commit().await.map_err(map_db_error)?;
    Ok(closed)
}

/// A one-shot step run after the first replay, before the break closes: lets a
/// test play the stream's part.
type Between = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Caller-fixable ingest failures are the caller's to see (422); a database
/// failure is ours.
fn ingest_error(e: IngestError) -> AdminError {
    let status = match e {
        IngestError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        IngestError::UnknownAccount { .. } | IngestError::Other(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    AdminError::new(status, format!("could not book the break: {e}"))
}

async fn adopt(state: &AppState, auth: &AuthContext, id: Uuid, portfolio: Uuid, mut between: Option<Between>) -> Result<ReconBreak, AdminError> {
    require_portfolio(state.pool(), portfolio).await?;
    let mut booked = 0usize;
    for _ in 0..ADOPT_ROUNDS {
        let found = open_break(state.pool(), id).await?;
        let events = found
            .events
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| {
                event_from_json(e)
                    .ok_or_else(|| AdminError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("unreadable event in the break: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let total = events.len();
        let connection = found.broker_connection_code.as_deref().ok_or_else(|| unprocessable("the break has no broker connection"))?;
        for event in events.into_iter().skip(booked) {
            let done = ingest_with_portfolio(state.pool(), state.event_bus(), connection, event, None, portfolio)
                .await
                .map_err(ingest_error)?;
            if let Decision::Break(kind) = done.decision {
                return Err(AdminError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("the break cannot be booked: {kind:?}; it stays open"),
                ));
            }
        }
        booked = total;
        if let Some(step) = between.take() {
            step.await;
        }
        if let Some(closed) = close_break(state.pool(), id, "resolved", principal_of(auth), Some(total as i32)).await? {
            return Ok(closed);
        }
    }
    Err(conflict("the break kept collecting events; adopt again"))
}

/// Close the break without booking anything.
pub async fn dismiss_break(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<ReconBreak> {
    open_break(state.pool(), id).await?;
    close_break(state.pool(), id, "dismissed", principal_of(&auth), None)
        .await?
        .map(Json)
        .ok_or_else(|| conflict("the break was resolved by someone else"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::{Extension, Json};
    use gateway::adapter::{BrokerAdapter, BrokerError, BrokerRegistry};
    use gateway::venue_event::{VenueEvent, VenueFill, VenueOrderStatus};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    use crate::auth::{AuthContext, AuthMethod};
    use crate::handlers::tests::test_app_state;
    use crate::ingest::apply::tests::{world, World};

    async fn principal(w: &World, role: Option<&str>) -> AuthContext {
        let id = Uuid::new_v4();
        let code = format!("p-{}", id.simple());
        sqlx::query("INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')")
            .bind(id).bind(&code).execute(&w.pool).await.unwrap();
        if let Some(role) = role {
            sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
                .bind(id).bind(role).execute(&w.pool).await.unwrap();
        }
        AuthContext { principal_id: id, principal_code: code, auth_method: AuthMethod::Oidc, mfa: true }
    }

    fn fill(exec: &str, venue_order: &str, qty: f64) -> VenueFill {
        VenueFill {
            exec_id: exec.into(),
            venue_order_id: venue_order.into(),
            client_order_id: None,
            account_ref: "acc".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            qty,
            price: 100.0,
            fee: None,
            executed_at: None,
        }
    }

    async fn other_portfolio(w: &World) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
            .bind(id).bind(format!("PF-{}", id.simple())).execute(&w.pool).await.unwrap();
        id
    }

    fn rule_body(account: Option<Uuid>, prefix: Option<&str>, portfolio: Uuid) -> RuleBody {
        RuleBody { account_id: account, client_id_prefix: prefix.map(str::to_string), portfolio_id: portfolio, priority: 5 }
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rule_without_account_or_prefix_is_400() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let err = create_rule(State(st.clone()), Path(w.conn.clone()), Json(rule_body(None, None, w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let err = create_rule(State(st.clone()), Path(w.conn.clone()), Json(rule_body(None, Some(""), w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        // unknown portfolio / account are 404
        let err = create_rule(State(st.clone()), Path(w.conn.clone()), Json(rule_body(None, Some("x-"), Uuid::new_v4()))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        let err = create_rule(State(st), Path(w.conn.clone()), Json(rule_body(Some(Uuid::new_v4()), None, w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rules_crud_round_trip() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let other = other_portfolio(&w).await;
        let (status, Json(created)) =
            create_rule(State(st.clone()), Path(w.conn.clone()), Json(rule_body(None, Some("alpha-"), w.portfolio))).await.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let Json(listed) = list_rules(State(st.clone()), Path(w.conn.clone())).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id, listed[0].client_id_prefix.as_deref(), listed[0].priority), (created.id, Some("alpha-"), 5));
        let Json(updated) = update_rule(
            State(st.clone()),
            Path((w.conn.clone(), created.id)),
            Json(RuleBody { priority: 1, ..rule_body(Some(w.account), None, other) }),
        )
        .await
        .unwrap();
        assert_eq!((updated.account_id, updated.client_id_prefix, updated.portfolio_id, updated.priority), (Some(w.account), None, other, 1));
        let gone = delete_rule(State(st.clone()), Path((w.conn.clone(), created.id))).await.unwrap();
        assert_eq!(gone, StatusCode::NO_CONTENT);
        let err = delete_rule(State(st), Path((w.conn.clone(), created.id))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn patch_recon_settings_persist_and_validate() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let other = other_portfolio(&w).await;
        let Json(s) = patch_recon(
            State(st.clone()),
            Path(w.conn.clone()),
            Json(ReconPatch {
                default_portfolio_id: Some(other),
                external_policy: Some("break".into()),
                eod_lookback: Some("1y".into()),
                intraday_poll_secs: Some(30),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            (s.default_portfolio_id, s.external_policy.as_str(), s.eod_lookback.as_str(), s.intraday_poll_secs),
            (Some(other), "break", "1y", Some(30))
        );
        // untouched fields stay
        let Json(s) = patch_recon(State(st.clone()), Path(w.conn.clone()), Json(ReconPatch { eod_lookback: Some("1d".into()), ..Default::default() }))
            .await
            .unwrap();
        assert_eq!((s.external_policy.as_str(), s.eod_lookback.as_str(), s.intraday_poll_secs), ("break", "1d", Some(30)));
        let bad = |p: ReconPatch| patch_recon(State(st.clone()), Path(w.conn.clone()), Json(p));
        let e = bad(ReconPatch { external_policy: Some("ignore".into()), ..Default::default() }).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let e = bad(ReconPatch { intraday_poll_secs: Some(0), ..Default::default() }).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let e = bad(ReconPatch { default_portfolio_id: Some(Uuid::new_v4()), ..Default::default() }).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
        let e = patch_recon(State(st), Path("nope".into()), Json(ReconPatch::default())).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn get_recon_settings_returns_current_values() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let other = other_portfolio(&w).await;
        patch_recon(
            State(st.clone()),
            Path(w.conn.clone()),
            Json(ReconPatch { default_portfolio_id: Some(other), eod_lookback: Some("5y".into()), intraday_poll_secs: Some(45), ..Default::default() }),
        )
        .await
        .unwrap();
        let Json(s) = get_recon(State(st.clone()), Path(w.conn.clone())).await.unwrap();
        assert_eq!(
            (s.code.as_str(), s.default_portfolio_id, s.external_policy.as_str(), s.eod_lookback.as_str(), s.intraday_poll_secs),
            (w.conn.as_str(), Some(other), "adopt", "5y", Some(45))
        );
        let e = get_recon(State(st), Path("nope".into())).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invalid_lookback_is_400() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let e = patch_recon(State(st.clone()), Path(w.conn.clone()), Json(ReconPatch { eod_lookback: Some("2d".into()), ..Default::default() }))
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let e = sweep(State(st.clone()), Path(w.conn.clone()), Ok(Json(SweepBody { lookback: Some("2d".into()) }))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        // valid but no live reconciler in a test process
        let e = sweep(State(st), Path(w.conn.clone()), Ok(Json(SweepBody { lookback: Some("1w".into()) }))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }

    /// Two fills (1 + 2) of one venue order, parked by policy break.
    async fn break_with_two_events(w: &World) -> Uuid {
        sqlx::query("UPDATE broker_connection SET external_policy = 'break' WHERE code = $1")
            .bind(&w.conn).execute(&w.pool).await.unwrap();
        crate::ingest::apply::ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Fill(fill("e1", "v1", 1.0)), None).await.unwrap();
        crate::ingest::apply::ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Fill(fill("e2", "v1", 2.0)), None).await.unwrap();
        sqlx::query_scalar("SELECT id FROM recon_break WHERE status = 'open'").fetch_one(&w.pool).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopting_a_break_books_its_fills_and_resolves_it() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let id = break_with_two_events(&w).await;
        let target = other_portfolio(&w).await;

        let Json(open) = list_breaks(State(st.clone()), Query(BreakFilter { status: Some("open".into()), ..Default::default() })).await.unwrap();
        assert_eq!((open.len(), open[0].id, open[0].events.as_array().unwrap().len()), (1, id, 2));

        let Json(done) = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(AdoptBody { portfolio_id: Some(target), ..Default::default() })).await.unwrap();
        assert_eq!((done.status.as_str(), done.resolved_by), ("resolved", Some(auth.principal_id)));
        assert!(done.resolved_at.is_some());

        let (origin, portfolio, cum, status): (String, Uuid, f64, String) =
            sqlx::query_as("SELECT origin, portfolio_id, cum_qty::float8, status FROM order_state").fetch_one(&w.pool).await.unwrap();
        assert_eq!((origin.as_str(), portfolio, cum, status.as_str()), ("EXTERNAL", target, 3.0, "filled"));
        let fills: i64 = sqlx::query_scalar("SELECT count(*) FROM venue_fill").fetch_one(&w.pool).await.unwrap();
        assert_eq!(fills, 2);
        let net: f64 = sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1")
            .bind(target).fetch_one(&w.pool).await.unwrap();
        assert_eq!(net, 3.0);

        // a second adopt is a conflict, not a double booking
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: Some(target), ..Default::default() })).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopted_break_order_is_observed() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        sqlx::query("UPDATE broker_connection SET external_policy = 'break' WHERE code = $1")
            .bind(&w.conn).execute(&w.pool).await.unwrap();
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc);
        let update = gateway::venue_event::VenueOrderUpdate {
            venue_order_id: "v1".into(),
            client_order_id: None,
            account_ref: "acc".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            order_type: "LIMIT".into(),
            qty: 5.0,
            limit_px: Some(100.0),
            status: VenueOrderStatus::Working,
            cum_qty: 0.0,
            ts: Some(at("2026-11-01T09:00:01Z")),
            created_at: Some(at("2026-11-01T09:00:00Z")),
        };
        crate::ingest::apply::ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Order(update), None).await.unwrap();
        let mut f = fill("e1", "v1", 2.0);
        f.executed_at = Some(at("2026-11-01T09:05:00Z"));
        crate::ingest::apply::ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Fill(f), None).await.unwrap();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM recon_break WHERE status = 'open'").fetch_one(&w.pool).await.unwrap();

        adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: Some(w.portfolio), ..Default::default() })).await.unwrap();

        let events: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as("SELECT event_type, occurred_at FROM order_event ORDER BY global_position")
                .fetch_all(&w.pool).await.unwrap();
        assert_eq!(
            events,
            vec![
                ("order_observed".into(), Some(at("2026-11-01T09:00:01Z"))),
                ("order_partially_filled".into(), Some(at("2026-11-01T09:05:00Z"))),
            ]
        );
        let (status, cum, created, last): (String, f64, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as("SELECT status, cum_qty::float8, venue_created_at, last_trade_at FROM order_state")
                .fetch_one(&w.pool).await.unwrap();
        assert_eq!(
            (status.as_str(), cum, created, last),
            ("partially_filled", 2.0, Some(at("2026-11-01T09:00:00Z")), Some(at("2026-11-01T09:05:00Z")))
        );
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopting_a_break_that_cannot_book_leaves_it_open() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let id = break_with_two_events(&w).await;
        // the symbol is not in the catalog: nothing can book
        sqlx::query("UPDATE recon_break SET events_json = replace(events_json::text, 'BTCUSDT', 'GONE')::jsonb")
            .execute(&w.pool).await.unwrap();
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: Some(w.portfolio), ..Default::default() })).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let status: String = sqlx::query_scalar("SELECT status FROM recon_break WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
        assert_eq!(status, "open");
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM order_state").fetch_one(&w.pool).await.unwrap();
        assert_eq!(orders, 0);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopt_books_events_attached_during_replay() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let id = break_with_two_events(&w).await;
        // the stream attaches a third fill after the snapshot, before the close
        let pool = w.pool.clone();
        let late = async move {
            let fill = serde_json::json!({
                "type": "fill", "exec_id": "e3", "venue_order_id": "v1", "client_order_id": null,
                "account_ref": "acc", "symbol": "BTCUSDT", "buy": true, "qty": 4.0, "price": 100.0,
                "fee": null, "executed_at": null,
            });
            sqlx::query("UPDATE recon_break SET events_json = events_json || jsonb_build_array($2::jsonb) WHERE id = $1")
                .bind(id).bind(fill).execute(&pool).await.unwrap();
        };
        let done = adopt(&st, &auth, id, w.portfolio, Some(Box::pin(late))).await.unwrap();
        assert_eq!((done.status.as_str(), done.events.as_array().unwrap().len()), ("resolved", 3));
        let (cum, fills): (f64, i64) = sqlx::query_as("SELECT (SELECT cum_qty::float8 FROM order_state), (SELECT count(*) FROM venue_fill)")
            .fetch_one(&w.pool).await.unwrap();
        assert_eq!((cum, fills), (7.0, 3));
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopting_a_break_of_an_unknown_account_is_422() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let id = break_with_two_events(&w).await;
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) VALUES ($1, 'ACC2', $2, 'acc2', 'ACTIVE')",
        )
        .bind(Uuid::new_v4()).bind(&w.conn).execute(&w.pool).await.unwrap();
        sqlx::query("UPDATE recon_break SET events_json = replace(events_json::text, '\"acc\"', '\"zzz\"')::jsonb")
            .execute(&w.pool).await.unwrap();
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: Some(w.portfolio), ..Default::default() })).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_malformed_sweep_body_is_400_and_no_body_is_the_default() {
        use axum::extract::FromRequest;
        let req = |ct: Option<&str>, body: &str| {
            let mut b = axum::http::Request::builder().method("POST").uri("/");
            if let Some(ct) = ct {
                b = b.header("content-type", ct);
            }
            b.body(axum::body::Body::from(body.to_string())).unwrap()
        };
        let bad = Json::<SweepBody>::from_request(req(Some("application/json"), "{nope"), &()).await;
        assert!(bad.is_err());
        let none = Json::<SweepBody>::from_request(req(None, ""), &()).await;
        assert!(matches!(none, Err(axum::extract::rejection::JsonRejection::MissingJsonContentType(_))));
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn dismissing_a_break_closes_it_without_booking() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let id = break_with_two_events(&w).await;
        let Json(done) = dismiss_break(State(st.clone()), Extension(auth.clone()), Path(id)).await.unwrap();
        assert_eq!((done.status.as_str(), done.resolved_by), ("dismissed", Some(auth.principal_id)));
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM order_state").fetch_one(&w.pool).await.unwrap();
        assert_eq!(orders, 0);
        let e = dismiss_break(State(st), Extension(auth), Path(id)).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }

    struct Recorder(Mutex<Vec<(String, String)>>);

    #[async_trait::async_trait]
    impl BrokerAdapter for Recorder {
        async fn submit_order(&self, _: &gateway::adapter::BrokerOrderRequest) -> Result<gateway::adapter::BrokerOrderResponse, BrokerError> {
            unreachable!()
        }
        async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
            self.0.lock().unwrap().push((external_order_id.into(), symbol.into()));
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn external_order_cancel_routes_by_venue_order_id() {
        let w = world().await;
        // an adopted, still-working external order
        crate::ingest::apply::ingest(
            &w.pool,
            &w.bus,
            &w.conn,
            VenueEvent::Order(gateway::venue_event::VenueOrderUpdate {
                venue_order_id: "v-ext".into(),
                client_order_id: None,
                account_ref: "acc".into(),
                symbol: "BTCUSDT".into(),
                buy: true,
                order_type: "LIMIT".into(),
                qty: 5.0,
                limit_px: Some(100.0),
                status: VenueOrderStatus::Working,
                cum_qty: 0.0,
                ts: None,
                created_at: None,
            }),
            None,
        )
        .await
        .unwrap();
        let order_id: Uuid = sqlx::query_scalar("SELECT order_id FROM order_state WHERE origin = 'EXTERNAL'").fetch_one(&w.pool).await.unwrap();

        let recorder = Arc::new(Recorder(Mutex::default()));
        let mut registry = BrokerRegistry::new();
        registry.register("BINANCE", "PAPER", recorder.clone());
        let st = test_app_state(w.pool.clone());
        st.swap_registries(registry, Default::default());

        let pm = principal(&w, Some("pm")).await;
        sqlx::query("INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) VALUES ($1, $2, $3, true, true)")
            .bind(Uuid::new_v4()).bind(pm.principal_id).bind(w.portfolio).execute(&w.pool).await.unwrap();

        let outcome = crate::order_entry::cancel(&st, &pm, order_id, None).await.unwrap();
        assert!(matches!(outcome, crate::order_entry::CancelOutcome::Requested));
        assert_eq!(*recorder.0.lock().unwrap(), vec![("v-ext".to_string(), "BTCUSDT".to_string())]);
    }

    #[test]
    fn venue_listing_marks_only_ibkr_multi_account() {
        let listing = crate::venues::venue_listing();
        assert!(!listing.is_empty());
        for v in &listing {
            let json = serde_json::to_value(v).unwrap();
            assert_eq!(json["multi_account"], matches!(json["code"].as_str().unwrap(), "IBKR" | "IBKR_TWS"), "{}", json["code"]);
            assert!(json["environments"].is_array(), "venue fields stay flattened");
        }
    }

    // ---- REPORTED breaks ----

    use crate::reported::ingest::{ingest as ingest_reported, FillBatch};
    use serde_json::json;

    struct Reported {
        strategy: String,
        portfolio: Uuid,
        instrument: i64,
    }

    /// A REPORTED strategy with its own portfolio, and an instrument no venue maps yet.
    async fn reported(w: &World) -> Reported {
        let n = Uuid::new_v4().simple().to_string();
        for sql in [
            "INSERT INTO venue (code, name) VALUES ('CRYPTO', 'c') ON CONFLICT DO NOTHING",
            "INSERT INTO currency (code, name) VALUES ('USDT', 'tether') ON CONFLICT DO NOTHING",
        ] {
            sqlx::query(sql).execute(&w.pool).await.unwrap();
        }
        let instrument: i64 = sqlx::query_scalar(
            "INSERT INTO instrument (symbol, name, venue, asset_class, instrument_class, currency, price_precision, price_increment) \
             VALUES ($1, 'x', 'CRYPTO', 'CRYPTO', 'SPOT', 'USDT', 2, 0.01) RETURNING id",
        )
        .bind(format!("RPT{n}")).fetch_one(&w.pool).await.unwrap();
        let owner = principal(w, None).await.principal_id;
        let portfolio = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
            .bind(portfolio).bind(format!("RPT-{n}")).execute(&w.pool).await.unwrap();
        let strategy = format!("rep-{n}");
        sqlx::query("INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, $1, $2, $3, 'REPORTED')")
            .bind(&strategy).bind(portfolio).bind(owner).execute(&w.pool).await.unwrap();
        Reported { strategy, portfolio, instrument }
    }

    fn reported_fill(exec: &str, qty: f64) -> serde_json::Value {
        json!({"venue": "ZEXCH", "account_ref": "main", "exec_id": exec, "symbol": "ZZZUSDT",
               "side": "BUY", "qty": qty, "price": 100.0, "trade_time": "2026-11-23T10:00:00Z"})
    }

    async fn report(w: &World, r: &Reported, batch: &str, fills: Vec<serde_json::Value>) -> crate::reported::ingest::BatchResult {
        ingest_reported(&w.pool, &r.strategy, FillBatch { schema_version: 1, batch_id: batch.into(), sequence: None, fills })
            .await.unwrap()
    }

    async fn reported_break(w: &World, r: &Reported, fill: serde_json::Value) -> Uuid {
        let res = report(w, r, &format!("b-{}", Uuid::new_v4()), vec![fill]).await;
        res.results[0].break_id.expect("a break")
    }

    fn adopt_into(r: &Reported) -> AdoptBody {
        AdoptBody { instrument_id: Some(r.instrument.to_string()), ..Default::default() }
    }

    async fn net_qty(w: &World, r: &Reported) -> f64 {
        sqlx::query_scalar::<_, f64>("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
            .bind(r.portfolio).bind(r.instrument.to_string()).fetch_optional(&w.pool).await.unwrap().unwrap_or(0.0)
    }

    async fn fill_rows(w: &World) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM reported_fill").fetch_one(&w.pool).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopt_reported_break_books_once() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 2.0)).await;
        assert_eq!(fill_rows(&w).await, 0);

        let Json(done) = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(adopt_into(&r))).await.unwrap();
        assert_eq!((done.status.as_str(), done.resolved_by), ("resolved", Some(auth.principal_id)));
        assert_eq!((fill_rows(&w).await, net_qty(&w, &r).await), (1, 2.0));

        let e = adopt_break(State(st), Extension(auth), Path(id), Json(adopt_into(&r))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
        assert_eq!((fill_rows(&w).await, net_qty(&w, &r).await), (1, 2.0));
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopted_mapping_books_later_fills() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 1.0)).await;
        let _ = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(adopt_into(&r))).await.unwrap();
        let later = report(&w, &r, "later", vec![reported_fill("e2", 3.0)]).await;
        assert_eq!(later.results[0].outcome.as_str(), "booked");
        assert_eq!(net_qty(&w, &r).await, 4.0);

        // without the mapping the next fill of another symbol still breaks
        let r2 = reported(&w).await;
        let mut other = reported_fill("e9", 1.0);
        other["symbol"] = json!("OTHERUSDT");
        let id2 = reported_break(&w, &r2, other.clone()).await;
        let body = AdoptBody { map_symbol: Some(false), ..adopt_into(&r2) };
        let _ = adopt_break(State(st), Extension(auth), Path(id2), Json(body)).await.unwrap();
        other["exec_id"] = json!("e10");
        let next = report(&w, &r2, "next", vec![other]).await;
        assert_eq!(next.results[0].outcome.as_str(), "break");
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopt_reported_break_into_another_portfolio_is_audited() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let target = other_portfolio(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 2.0)).await;
        let body = AdoptBody { portfolio_id: Some(target), ..adopt_into(&r) };
        let _ = adopt_break(State(st), Extension(auth.clone()), Path(id), Json(body)).await.unwrap();
        let net: f64 = sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1")
            .bind(target).fetch_one(&w.pool).await.unwrap();
        assert_eq!(net, 2.0);
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_event WHERE action = 'recon.reported_break_rebooked' AND actor = $1 AND detail->>'break_id' = $2",
        )
        .bind(&auth.principal_code).bind(id.to_string()).fetch_one(&w.pool).await.unwrap();
        assert_eq!(audited, 1);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopt_reported_break_needs_a_known_instrument() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 1.0)).await;
        let e = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(AdoptBody::default())).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let body = AdoptBody { instrument_id: Some("999999999".into()), ..Default::default() };
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(body)).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
        let open: String = sqlx::query_scalar("SELECT status FROM recon_break WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
        assert_eq!((open.as_str(), fill_rows(&w).await), ("open", 0));
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopting_an_invalid_fill_is_422() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let mut bad = reported_fill("e1", 1.0);
        bad.as_object_mut().unwrap().remove("price");
        let id = reported_break(&w, &r, bad).await;
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(adopt_into(&r))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let status: String = sqlx::query_scalar("SELECT status FROM recon_break WHERE id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
        assert_eq!((status.as_str(), fill_rows(&w).await), ("open", 0));
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopting_a_conflicting_duplicate_is_422_and_dismiss_works() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol) VALUES ($1, 'ZEXCH', 'ZZZUSDT')")
            .bind(r.instrument).execute(&w.pool).await.unwrap();
        let first = report(&w, &r, "b1", vec![reported_fill("e1", 1.0)]).await;
        assert_eq!(first.results[0].outcome.as_str(), "booked");
        let id = reported_break(&w, &r, reported_fill("e1", 5.0)).await;
        let e = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(adopt_into(&r))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!((fill_rows(&w).await, net_qty(&w, &r).await), (1, 1.0));
        let Json(done) = dismiss_break(State(st), Extension(auth), Path(id)).await.unwrap();
        assert_eq!(done.status, "dismissed");
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn dismiss_reported_break_books_nothing() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 1.0)).await;
        let Json(done) = dismiss_break(State(st.clone()), Extension(auth.clone()), Path(id)).await.unwrap();
        assert_eq!((done.status.as_str(), done.source.as_str()), ("dismissed", "REPORTED"));
        assert_eq!((fill_rows(&w).await, net_qty(&w, &r).await), (0, 0.0));
        let e = dismiss_break(State(st), Extension(auth), Path(id)).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn list_breaks_filters_by_source() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let r = reported(&w).await;
        let reconciled = break_with_two_events(&w).await;
        let reported_id = reported_break(&w, &r, reported_fill("e1", 1.0)).await;

        let list = |source: Option<&str>| {
            let st = st.clone();
            let source = source.map(str::to_string);
            async move { list_breaks(State(st), Query(BreakFilter { source, ..Default::default() })).await.map(|Json(b)| b) }
        };
        let all = list(None).await.unwrap();
        assert_eq!(all.len(), 2, "both kinds list together");
        let rep = list(Some("REPORTED")).await.unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!((rep[0].id, rep[0].strategy_code.as_deref(), rep[0].broker_connection_code.as_deref()), (reported_id, Some(r.strategy.as_str()), None));
        assert_eq!(rep[0].reported_fill.as_ref().unwrap()["exec_id"], "e1");
        let rec = list(Some("RECONCILED")).await.unwrap();
        assert_eq!((rec.len(), rec[0].id), (1, reconciled));
        assert!(rec[0].broker_connection_code.is_some() && rec[0].reported_fill.is_none());
        assert_eq!(list(Some("nope")).await.unwrap_err().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rebooked_fill_survives_rebuild() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let target = other_portfolio(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 2.0)).await;
        let body = AdoptBody { portfolio_id: Some(target), ..adopt_into(&r) };
        let _ = adopt_break(State(st), Extension(auth), Path(id), Json(body)).await.unwrap();
        for scope in [Some(target), Some(r.portfolio), None] {
            crate::positions::rebuild_positions_scoped(&w.pool, scope).await.unwrap();
            let net: Option<f64> = sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
                .bind(target).bind(r.instrument.to_string()).fetch_optional(&w.pool).await.unwrap();
            assert_eq!(net, Some(2.0), "override portfolio after rebuild {scope:?}");
            assert_eq!(net_qty(&w, &r).await, 0.0, "strategy portfolio untouched after rebuild {scope:?}");
        }
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn adopt_conflicting_mapping_is_409() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let auth = principal(&w, None).await;
        let r = reported(&w).await;
        let other = reported(&w).await;
        let id = reported_break(&w, &r, reported_fill("e1", 1.0)).await;
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol) VALUES ($1, 'ZEXCH', 'ZZZUSDT')")
            .bind(other.instrument).execute(&w.pool).await.unwrap();
        let e = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(adopt_into(&r))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
        assert_eq!(fill_rows(&w).await, 0);
        // map_symbol=false books just this fill
        let body = AdoptBody { map_symbol: Some(false), ..adopt_into(&r) };
        let _ = adopt_break(State(st), Extension(auth), Path(id), Json(body)).await.unwrap();
        assert_eq!(fill_rows(&w).await, 1);
    }
}
