//! `GET /futures/roots`: every active root with its front and next contract.
//! `GET /futures/roll-targets`: what each held futures position would roll into.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::front::{front_and_next, roll_date, venue_today, FutureContract};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::authz::{self, Permission};
use crate::handlers::ApiError;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FutureRootView {
    pub code: String,
    pub name: String,
    pub venue: String,
    /// Days before a contract's roll date from which the cockpit flags a held position.
    pub roll_warn_days: i32,
    /// Price increment of the root's calendar spreads (a roll's limit price), e.g. 0.05 for ES.
    pub spread_tick: f64,
    pub front: Option<FutureContract>,
    pub next: Option<FutureContract>,
}

/// List futures roots with their front and next contract
#[utoipa::path(
    get, path = "/futures/roots", tag = "orders",
    responses(
        (status = 200, description = "Active futures roots with front and next contract", body = [FutureRootView]),
        (status = 401, description = "No credential"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_roots(State(state): State<AppState>) -> Result<Json<Vec<FutureRootView>>, ApiError> {
    let fail = |e: sqlx::Error| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list futures roots: {e:?}"),
    };
    let roots: Vec<(String, String, String, i32, f64)> = sqlx::query_as(
        "SELECT code, name, venue, roll_warn_days, spread_tick::float8 FROM future_root WHERE status = 'ACTIVE' ORDER BY code",
    )
            .fetch_all(state.pool())
            .await
            .map_err(fail)?;
    let today = venue_today(chrono::Utc::now());
    let mut out = Vec::with_capacity(roots.len());
    for (code, name, venue, roll_warn_days, spread_tick) in roots {
        let (front, next) = front_and_next(state.pool(), &code, today).await.map_err(fail)?;
        out.push(FutureRootView { code, name, venue, roll_warn_days, spread_tick, front, next });
    }
    Ok(Json(out))
}

/// A listed contract, by id and symbol.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub struct ContractRef {
    pub instrument_id: i64,
    pub symbol: String,
}

/// A held futures position and the contract it rolls into.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub struct RollTargetView {
    pub portfolio_id: Uuid,
    /// The held contract.
    pub instrument_id: i64,
    pub symbol: String,
    pub root: String,
    pub net_qty: f64,
    /// The held contract's roll date (venue-local); past it the roll is overdue.
    pub roll_date: NaiveDate,
    /// Days before `roll_date` from which the cockpit flags the position.
    pub roll_warn_days: i32,
    /// The root's next listed ACTIVE contract after the held one.
    pub next: ContractRef,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct RollTargetsQuery {
    /// Only this portfolio's positions (default: every portfolio the caller can view).
    pub portfolio_id: Option<Uuid>,
}

/// Held futures positions (on `portfolios`, or all when `None`) with the
/// contract each rolls into. Positions whose root lists no later ACTIVE
/// contract are left out: there is nothing to roll into.
pub async fn roll_targets(pool: &sqlx::PgPool, portfolios: &[Uuid]) -> Result<Vec<RollTargetView>, sqlx::Error> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(Uuid, i64, String, String, f64, NaiveDate, i32, i32, i64, String)> = sqlx::query_as(
        "SELECT p.portfolio_id, i.id, i.symbol, d.underlying_symbol, p.net_qty::float8, d.expiry_date, \
                r.roll_offset_days, r.roll_warn_days, n.id, n.symbol \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         JOIN instrument_derivative d ON d.instrument_id = i.id \
         JOIN future_root r ON r.code = d.underlying_symbol \
         JOIN LATERAL ( \
           SELECT ni.id, ni.symbol FROM instrument ni \
           JOIN instrument_derivative nd ON nd.instrument_id = ni.id \
           WHERE ni.instrument_class = 'FUTURE' AND ni.status = 'ACTIVE' \
             AND nd.underlying_symbol = d.underlying_symbol AND nd.expiry_date > d.expiry_date \
           ORDER BY nd.expiry_date, ni.id LIMIT 1) n ON true \
         WHERE p.net_qty <> 0 AND p.portfolio_id = ANY($1) \
           AND i.instrument_class = 'FUTURE' AND i.status = 'ACTIVE' AND d.expiry_date IS NOT NULL \
         ORDER BY p.portfolio_id, d.expiry_date, i.id",
    )
    .bind(portfolios)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(portfolio_id, instrument_id, symbol, root, net_qty, expiry, offset, roll_warn_days, next_id, next_symbol)| {
            RollTargetView {
                portfolio_id,
                instrument_id,
                symbol,
                root,
                net_qty,
                roll_date: roll_date(expiry, offset),
                roll_warn_days,
                next: ContractRef { instrument_id: next_id, symbol: next_symbol },
            }
        })
        .collect())
}

/// List the roll target of each held futures position
#[utoipa::path(
    get, path = "/futures/roll-targets", tag = "orders",
    params(RollTargetsQuery),
    responses(
        (status = 200, description = "Held futures positions on the portfolios the caller can view, each with the contract it rolls into (overdue contracts included)", body = [RollTargetView]),
        (status = 401, description = "No credential"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_roll_targets(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<RollTargetsQuery>,
) -> Result<Json<Vec<RollTargetView>>, ApiError> {
    let fail = |e: sqlx::Error| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list roll targets: {e:?}"),
    };
    let mut visible = authz::visible_portfolios(state.pool(), auth.principal_id, Permission::PositionsView).await.map_err(fail)?;
    if let Some(pf) = q.portfolio_id {
        visible.retain(|p| *p == pf);
    }
    Ok(Json(roll_targets(state.pool(), &visible).await.map_err(fail)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn future(pool: &sqlx::PgPool, symbol: &str, expiry: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, contract_size, price_precision, price_increment) \
             VALUES ($1, 'XCME', $1, 'INDEX', 'FUTURE', 'USD', 'ACTIVE', 50, 2, 0.25) RETURNING id",
        )
        .bind(symbol)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO instrument_derivative (instrument_id, underlying_symbol, expiry_date) VALUES ($1, 'ES', $2::date)")
            .bind(id)
            .bind(expiry)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn hold(pool: &sqlx::PgPool, pf: Uuid, instrument: i64, qty: f64) {
        sqlx::query("INSERT INTO position (portfolio_id, instrument_id, net_qty) VALUES ($1, $2, $3)")
            .bind(pf)
            .bind(instrument.to_string())
            .bind(qty)
            .execute(pool)
            .await
            .unwrap();
    }

    /// The roll target does not depend on the day: an overdue contract (no
    /// longer front, its alert perhaps dismissed) still names the next one.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn each_held_future_names_its_next_contract() {
        let pool = crate::authz_schema_tests::migrated_db().await;
        let state = crate::strategies::test_support::state(pool.clone());
        let pf = crate::groups_api::tests::portfolio(&pool).await;
        let other = crate::groups_api::tests::portfolio(&pool).await;
        let me = crate::order_entry::tests::g_trader(&pool, pf).await;
        // a contract long past its roll date, its successor, and the last listed one
        let old = future(&pool, "ESZ5", "2025-12-19").await;
        let next = future(&pool, "ESH6", "2026-03-20").await;
        let last = future(&pool, "ESM6", "2026-06-19").await;
        let expired = future(&pool, "ESU5", "2025-09-19").await;
        sqlx::query("UPDATE instrument SET status = 'EXPIRED' WHERE id = $1").bind(expired).execute(&pool).await.unwrap();
        hold(&pool, pf, old, -3.0).await;
        hold(&pool, pf, last, 1.0).await; // nothing listed after it
        hold(&pool, pf, next, 0.0).await; // flat
        hold(&pool, pf, expired, 2.0).await; // expired: nothing to roll
        hold(&pool, other, old, 5.0).await; // a portfolio I cannot see
        let call = |portfolio_id: Option<Uuid>| {
            list_roll_targets(State(state.clone()), Extension(me.clone()), Query(RollTargetsQuery { portfolio_id }))
        };
        let Json(all) = call(None).await.unwrap();
        assert_eq!(
            all,
            vec![RollTargetView {
                portfolio_id: pf,
                instrument_id: old,
                symbol: "ESZ5".into(),
                root: "ES".into(),
                net_qty: -3.0,
                roll_date: NaiveDate::from_ymd_opt(2025, 12, 11).unwrap(),
                roll_warn_days: 3,
                next: ContractRef { instrument_id: next, symbol: "ESH6".into() },
            }]
        );
        let Json(mine) = call(Some(pf)).await.unwrap();
        assert_eq!(mine, all);
        let Json(theirs) = call(Some(other)).await.unwrap();
        assert!(theirs.is_empty(), "{theirs:?}");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn roots_carry_their_spread_tick() {
        let pool = crate::authz_schema_tests::migrated_db().await;
        let state = crate::strategies::test_support::state(pool.clone());
        let Json(roots) = list_roots(State(state)).await.unwrap();
        let ticks: Vec<(String, f64)> = roots.into_iter().map(|r| (r.code, r.spread_tick)).collect();
        assert_eq!(ticks, vec![("ES".into(), 0.05), ("MES".into(), 0.05), ("NQ".into(), 0.05)]);
    }
}
