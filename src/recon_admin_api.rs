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
    pub broker_connection_code: String,
    pub account_id: Uuid,
    pub venue_order_id: String,
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
    "id, broker_connection_code, account_id, venue_order_id, kind, events_json, status, created_at, resolved_at, resolved_by";

#[derive(Debug, Default, Deserialize)]
pub struct BreakFilter {
    /// `open` | `resolved` | `dismissed`; all when left out.
    pub status: Option<String>,
}

pub async fn list_breaks(State(state): State<AppState>, Query(filter): Query<BreakFilter>) -> ApiResult<Vec<ReconBreak>> {
    if filter.status.as_deref().is_some_and(|s| !matches!(s, "open" | "resolved" | "dismissed")) {
        return Err(bad_request("status must be open, resolved or dismissed"));
    }
    let breaks = sqlx::query_as::<_, ReconBreak>(&format!(
        "SELECT {BREAK_COLUMNS} FROM recon_break WHERE ($1::text IS NULL OR status = $1) ORDER BY created_at DESC LIMIT 500"
    ))
    .bind(filter.status)
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

#[derive(Debug, Deserialize)]
pub struct AdoptBody {
    pub portfolio_id: Uuid,
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
    adopt(&state, &auth, id, body.portfolio_id, None).await.map(Json)
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
        for event in events.into_iter().skip(booked) {
            let done = ingest_with_portfolio(state.pool(), state.event_bus(), &found.broker_connection_code, event, None, portfolio)
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
    use crate::ingest::apply::tests::{world, World, CONN};

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
        let err = create_rule(State(st.clone()), Path(CONN.into()), Json(rule_body(None, None, w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let err = create_rule(State(st.clone()), Path(CONN.into()), Json(rule_body(None, Some(""), w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        // unknown portfolio / account are 404
        let err = create_rule(State(st.clone()), Path(CONN.into()), Json(rule_body(None, Some("x-"), Uuid::new_v4()))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        let err = create_rule(State(st), Path(CONN.into()), Json(rule_body(Some(Uuid::new_v4()), None, w.portfolio))).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rules_crud_round_trip() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let other = other_portfolio(&w).await;
        let (status, Json(created)) =
            create_rule(State(st.clone()), Path(CONN.into()), Json(rule_body(None, Some("alpha-"), w.portfolio))).await.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let Json(listed) = list_rules(State(st.clone()), Path(CONN.into())).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id, listed[0].client_id_prefix.as_deref(), listed[0].priority), (created.id, Some("alpha-"), 5));
        let Json(updated) = update_rule(
            State(st.clone()),
            Path((CONN.into(), created.id)),
            Json(RuleBody { priority: 1, ..rule_body(Some(w.account), None, other) }),
        )
        .await
        .unwrap();
        assert_eq!((updated.account_id, updated.client_id_prefix, updated.portfolio_id, updated.priority), (Some(w.account), None, other, 1));
        let gone = delete_rule(State(st.clone()), Path((CONN.into(), created.id))).await.unwrap();
        assert_eq!(gone, StatusCode::NO_CONTENT);
        let err = delete_rule(State(st), Path((CONN.into(), created.id))).await.unwrap_err();
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
            Path(CONN.into()),
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
        let Json(s) = patch_recon(State(st.clone()), Path(CONN.into()), Json(ReconPatch { eod_lookback: Some("1d".into()), ..Default::default() }))
            .await
            .unwrap();
        assert_eq!((s.external_policy.as_str(), s.eod_lookback.as_str(), s.intraday_poll_secs), ("break", "1d", Some(30)));
        let bad = |p: ReconPatch| patch_recon(State(st.clone()), Path(CONN.into()), Json(p));
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
            Path(CONN.into()),
            Json(ReconPatch { default_portfolio_id: Some(other), eod_lookback: Some("5y".into()), intraday_poll_secs: Some(45), ..Default::default() }),
        )
        .await
        .unwrap();
        let Json(s) = get_recon(State(st.clone()), Path(CONN.into())).await.unwrap();
        assert_eq!(
            (s.code.as_str(), s.default_portfolio_id, s.external_policy.as_str(), s.eod_lookback.as_str(), s.intraday_poll_secs),
            (CONN, Some(other), "adopt", "5y", Some(45))
        );
        let e = get_recon(State(st), Path("nope".into())).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invalid_lookback_is_400() {
        let w = world().await;
        let st = test_app_state(w.pool.clone());
        let e = patch_recon(State(st.clone()), Path(CONN.into()), Json(ReconPatch { eod_lookback: Some("2d".into()), ..Default::default() }))
            .await
            .unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let e = sweep(State(st.clone()), Path(CONN.into()), Ok(Json(SweepBody { lookback: Some("2d".into()) }))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        // valid but no live reconciler in a test process
        let e = sweep(State(st), Path(CONN.into()), Ok(Json(SweepBody { lookback: Some("1w".into()) }))).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
    }

    /// Two fills (1 + 2) of one venue order, parked by policy break.
    async fn break_with_two_events(w: &World) -> Uuid {
        sqlx::query("UPDATE broker_connection SET external_policy = 'break' WHERE code = $1")
            .bind(CONN).execute(&w.pool).await.unwrap();
        crate::ingest::apply::ingest(&w.pool, &w.bus, CONN, VenueEvent::Fill(fill("e1", "v1", 1.0)), None).await.unwrap();
        crate::ingest::apply::ingest(&w.pool, &w.bus, CONN, VenueEvent::Fill(fill("e2", "v1", 2.0)), None).await.unwrap();
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

        let Json(open) = list_breaks(State(st.clone()), Query(BreakFilter { status: Some("open".into()) })).await.unwrap();
        assert_eq!((open.len(), open[0].id, open[0].events.as_array().unwrap().len()), (1, id, 2));

        let Json(done) = adopt_break(State(st.clone()), Extension(auth.clone()), Path(id), Json(AdoptBody { portfolio_id: target })).await.unwrap();
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
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: target })).await.unwrap_err();
        assert_eq!(e.status(), StatusCode::CONFLICT);
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
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: w.portfolio })).await.unwrap_err();
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
        .bind(Uuid::new_v4()).bind(CONN).execute(&w.pool).await.unwrap();
        sqlx::query("UPDATE recon_break SET events_json = replace(events_json::text, '\"acc\"', '\"zzz\"')::jsonb")
            .execute(&w.pool).await.unwrap();
        let e = adopt_break(State(st), Extension(auth), Path(id), Json(AdoptBody { portfolio_id: w.portfolio })).await.unwrap_err();
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
            CONN,
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
}
