//! Kill, Enable and Flatten, and the cancels that follow a session's end.
//!
//! None of these needs the strategy's cooperation: the OMS cancels through its
//! own venue connections, as the strategy's principal, on the one order path.

use serde::Serialize;
use sqlx::Row;
use tracing::{info, warn};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};
use crate::handlers::{ApiError, SubmitOrderRequest};
use crate::order_entry::{self, Origin};

use super::guard::{self, MarkLookup};
use super::session::{auth, session_key};
use super::{Mode, Strategy};

/// What a control action did, for the cockpit.
#[derive(Debug, Default, Serialize, utoipa::ToSchema)]
pub struct ControlReport {
    pub cancelled: usize,
    pub cancel_failures: Vec<String>,
    pub flatten_orders: usize,
    pub skipped: Vec<String>,
}

/// The closing order for a position: opposite side, its whole size, priced at
/// the band's edge on the aggressive side.
pub fn flatten_order(net_qty: f64, mid: f64, band_pct: f64) -> (OrderSide, f64, f64) {
    if net_qty > 0.0 {
        (OrderSide::Sell, net_qty, mid * (100.0 - band_pct) / 100.0)
    } else {
        (OrderSide::Buy, -net_qty, mid * (100.0 + band_pct) / 100.0)
    }
}

/// Cancel the strategy's working orders; Flatten's own orders stay when
/// `keep_flatten`.
pub async fn cancel_open(state: &AppState, strategy: &Strategy, keep_flatten: bool, reason: &str) -> ControlReport {
    let mut report = ControlReport::default();
    // List under the strategy row's lock, which every submit holds through
    // its first transaction: an order already past its checks is listed, and
    // one that is not yet sees the new state or the ended session.
    let open: Result<Vec<Uuid>, sqlx::Error> = async {
        let mut tx = state.pool().begin().await?;
        sqlx::query("SELECT 1 FROM strategy WHERE code = $1 FOR UPDATE").bind(&strategy.code).execute(&mut *tx).await?;
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT order_id FROM order_state WHERE portfolio_id = $1 \
               AND status IN ('submitted', 'routed', 'partially_filled', 'suspended') \
               AND NOT ($2 AND origin = 'FLATTEN')",
        )
        .bind(strategy.portfolio_id)
        .bind(keep_flatten)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ids)
    }
    .await;
    let open = match open {
        Ok(ids) => ids,
        Err(e) => {
            report.cancel_failures.push(format!("could not list open orders: {e}"));
            return report;
        }
    };
    for order_id in open {
        match order_entry::cancel(state, &auth(strategy), order_id, Some(reason.to_string())).await {
            Ok(_) => report.cancelled += 1,
            Err(e) => report.cancel_failures.push(format!("{order_id}: {}", e.message)),
        }
    }
    info!(strategy = %strategy.code, cancelled = report.cancelled, failures = report.cancel_failures.len(), %reason, "strategy: open orders cancelled");
    report
}

async fn set_state(state: &AppState, code: &str, trading_state: &str, rest_enabled: bool) -> Result<Strategy, ApiError> {
    let done = sqlx::query("UPDATE strategy SET state = $2, rest_enabled = $3, updated_at = now() WHERE code = $1")
        .bind(code)
        .bind(trading_state)
        .bind(rest_enabled)
        .execute(state.pool())
        .await
        .map_err(|e| ApiError { status: axum::http::StatusCode::INTERNAL_SERVER_ERROR, message: format!("could not set strategy state: {e}") })?;
    if done.rows_affected() == 0 {
        return Err(ApiError { status: axum::http::StatusCode::NOT_FOUND, message: "strategy not found".into() });
    }
    state.strategies().reload(state.pool()).await.map_err(|e| ApiError {
        status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("could not reload strategies: {e}"),
    })?;
    state.strategies().by_code(code).ok_or(ApiError { status: axum::http::StatusCode::NOT_FOUND, message: "strategy not found".into() })
}

/// Whether phase 7's automatic cancels (on session end, at boot) apply: not to
/// co-located strategies, whose own dead man's switch does that job.
pub fn phase7_cancels_apply(mode: Mode) -> bool {
    mode == Mode::ViaOms
}

/// Flatten is for strategies trading through the OMS only.
pub fn flatten_allowed(mode: Mode) -> Result<(), ApiError> {
    match mode {
        Mode::ViaOms => Ok(()),
        Mode::Colocated => Err(ApiError { status: axum::http::StatusCode::BAD_REQUEST, message: "co-located strategies have no Flatten".into() }),
    }
}

fn deployment_error(e: crate::colo::deployment::DeploymentError) -> ApiError {
    let status = if matches!(e, crate::colo::deployment::DeploymentError::Database(_)) {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    } else {
        axum::http::StatusCode::BAD_REQUEST
    };
    ApiError { status, message: e.to_string() }
}

/// Halt, stop REST, cancel every open order, log the session out. A
/// co-located strategy is cancelled at its venue and told over its link. The
/// durable kill command is written before the link is told, so an ACK cannot
/// outlive the latch.
pub async fn kill(state: &AppState, code: &str) -> Result<ControlReport, ApiError> {
    let mode = state.strategies().by_code(code).map(|s| s.mode);
    if mode == Some(Mode::Colocated) {
        if let Some(receipt) = crate::colo::deployment::record_kill_for_strategy(state.pool(), code)
            .await
            .map_err(deployment_error)?
        {
            state.colo().push_v2_control(
                receipt.deployment_id,
                colo_link::v2::ToRuntime::Kill {
                    kill: colo_link::v2::Kill { command_id: receipt.command_id },
                },
            );
        }
    }
    crate::colo::control::announce_kill(state.colo(), mode, code);
    let strategy = set_state(state, code, "HALTED", false).await?;
    if strategy.mode == Mode::Colocated {
        return Ok(crate::colo::control::kill(state, &strategy).await);
    }
    let report = cancel_open(state, &strategy, false, "strategy killed").await;
    state.fix_sessions().logout(&session_key(&strategy), "killed");
    Ok(report)
}

pub async fn enable(state: &AppState, code: &str) -> Result<(), ApiError> {
    let strategy = set_state(state, code, "ACTIVE", true).await?;
    if strategy.mode == Mode::Colocated {
        if let Some(receipt) = crate::colo::deployment::record_enable_for_strategy(state.pool(), code)
            .await
            .map_err(deployment_error)?
        {
            state.colo().push_v2_control(
                receipt.deployment_id,
                colo_link::v2::ToRuntime::Enable {
                    enable: colo_link::v2::Enable {
                        stopped_epoch: receipt.stopped_epoch.unwrap_or(0),
                        command_id: receipt.command_id,
                    },
                },
            );
        }
        crate::colo::control::enable(state, &strategy).await;
        return Ok(());
    }
    state.fix_sessions().enable(&session_key(&strategy));
    Ok(())
}

/// Approve the venue's current balances as a co-located strategy's expected
/// inventory, after a withdrawal, deposit or manual trade. Lifts an inventory
/// suspension once the runtime confirms.
pub async fn rebaseline(state: &AppState, code: &str) -> Result<String, ApiError> {
    let mode = state.strategies().by_code(code).map(|s| s.mode);
    let not_found = || ApiError { status: axum::http::StatusCode::NOT_FOUND, message: format!("strategy {code} not found") };
    match mode {
        None => return Err(not_found()),
        Some(Mode::ViaOms) => {
            return Err(ApiError {
                status: axum::http::StatusCode::BAD_REQUEST,
                message: "only co-located strategies keep an inventory to rebaseline".into(),
            })
        }
        Some(Mode::Colocated) => {}
    }
    let receipt = crate::colo::deployment::record_rebaseline_for_strategy(state.pool(), code)
        .await
        .map_err(deployment_error)?
        .ok_or_else(|| ApiError {
            status: axum::http::StatusCode::BAD_REQUEST,
            message: format!("strategy {code} has no active deployment"),
        })?;
    state.colo().push_v2_control(
        receipt.deployment_id,
        colo_link::v2::ToRuntime::Rebaseline {
            rebaseline: colo_link::v2::Rebaseline { command_id: receipt.command_id.clone() },
        },
    );
    Ok(receipt.command_id)
}

/// Kill, but leave the strategy reducing only, then close every position
/// with a limit order inside the price band.
pub async fn flatten(state: &AppState, code: &str) -> Result<ControlReport, ApiError> {
    let mode = state.strategies().by_code(code).map_or(Mode::ViaOms, |s| s.mode);
    flatten_allowed(mode)?;
    let strategy = set_state(state, code, "REDUCING", false).await?;
    let mut report = cancel_open(state, &strategy, false, "strategy flattened").await;
    state.fix_sessions().logout(&session_key(&strategy), "flattened");

    let positions = sqlx::query(
        "SELECT p.instrument_id, p.net_qty::double precision AS net_qty, i.id, i.symbol, i.instrument_class, i.asset_class, i.venue \
         FROM position p JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.portfolio_id = $1 AND p.net_qty <> 0",
    )
    .bind(strategy.portfolio_id)
    .fetch_all(state.pool())
    .await
    .map_err(|e| ApiError { status: axum::http::StatusCode::INTERNAL_SERVER_ERROR, message: format!("could not read positions: {e}") })?;

    for row in positions {
        let inst = dataprovider::InstrumentRef {
            id: row.get("id"),
            symbol: row.get("symbol"),
            instrument_class: row.get("instrument_class"),
            asset_class: row.get("asset_class"),
            venue: row.get("venue"),
        };
        let net_qty: f64 = row.get("net_qty");
        let Some(band) = strategy.price_band_pct else {
            report.skipped.push(format!("{}: no price band set", inst.symbol));
            continue;
        };
        let lookup = guard::reference_mark(
            state.marks(),
            &state.market_data().handle(),
            &guard::md_owner(&strategy.code),
            &inst,
            guard::covered(&inst),
            guard::HOLD_FOR_MARK,
        )
        .await;
        let MarkLookup::Live(mark) = lookup else {
            report.skipped.push(format!("{}: no reference price", inst.symbol));
            continue;
        };
        let account: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM order_state WHERE portfolio_id = $1 AND instrument_id = $2 AND cum_qty > 0 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(strategy.portfolio_id)
        .bind(inst.id.to_string())
        .fetch_optional(state.pool())
        .await
        .unwrap_or(None);
        let Some(account) = account else {
            report.skipped.push(format!("{}: no account has traded it", inst.symbol));
            continue;
        };
        let (side, quantity, limit) = flatten_order(net_qty, mark.mid(), band);
        let req = SubmitOrderRequest {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: format!("flatten-{}", strategy.code),
            portfolio_id: strategy.portfolio_id.to_string(),
            account_id: Some(account.to_string()),
            instrument_id: Some(inst.id.to_string()),
            symbol: None,
            venue: None,
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            limit_price: Some(limit),
            quantity,
        };
        match order_entry::submit(state, &auth(&strategy), req, Origin::Flatten).await {
            Ok(_) => report.flatten_orders += 1,
            Err(e) => report.skipped.push(format!("{}: {}", inst.symbol, e.message)),
        }
    }
    Ok(report)
}

/// A strategy's FIX session ended: cancel its working orders (Flatten's stay)
/// and stop pricing what only it wanted.
pub async fn on_session_end(state: &AppState, strategy: &Strategy) -> ControlReport {
    if !phase7_cancels_apply(strategy.mode) {
        return ControlReport::default();
    }
    let report = cancel_open(state, strategy, true, "FIX session ended").await;
    state.market_data().handle().unsubscribe_owner(&guard::md_owner(&strategy.code));
    report
}

/// At boot no strategy has a session, so none may have working orders
/// (Flatten's excepted).
pub async fn boot_recovery(state: &AppState) {
    for strategy in state.strategies().all().iter().filter(|s| phase7_cancels_apply(s.mode)) {
        let report = cancel_open(state, strategy, true, "OMS restarted").await;
        if !report.cancel_failures.is_empty() {
            warn!(strategy = %strategy.code, failures = ?report.cancel_failures, "strategy: some open orders were not cancelled at boot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leftover orders are cancelled before strategies can log on again, so a
    /// fast reconnect's new orders are never taken for leftovers.
    #[test]
    fn boot_recovery_runs_before_the_acceptor_opens() {
        let main = include_str!("../main.rs");
        let recovery = main.find("strategies::control::boot_recovery(&state)").expect("boot recovery is called");
        let acceptor = main.find("state.fix_sessions().start(").expect("the acceptor is started");
        assert!(recovery < acceptor);
    }

    #[test]
    fn colo_strategies_get_no_phase7_cancels() {
        assert!(phase7_cancels_apply(crate::strategies::Mode::ViaOms));
        assert!(!phase7_cancels_apply(crate::strategies::Mode::Colocated));
    }

    #[test]
    fn flatten_refuses_a_colocated_strategy() {
        let e = flatten_allowed(crate::strategies::Mode::Colocated).unwrap_err();
        assert_eq!(e.message, "co-located strategies have no Flatten");
        assert!(flatten_allowed(crate::strategies::Mode::ViaOms).is_ok());
    }

    #[test]
    fn flatten_sells_a_long_at_the_lower_band_edge() {
        assert_eq!(flatten_order(2.0, 100.0, 5.0), (OrderSide::Sell, 2.0, 95.0));
    }

    #[test]
    fn flatten_buys_a_short_at_the_upper_band_edge() {
        assert_eq!(flatten_order(-3.0, 100.0, 5.0), (OrderSide::Buy, 3.0, 105.0));
    }

    /// Boot recovery cancels every strategy's open orders in the database, so
    /// these tests, which each seed open orders, must not run at the same time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn setup() -> (sqlx::PgPool, String, AppState, Uuid, Uuid) {
        use crate::strategies::test_support as t;
        let pool = t::dev_pool().await;
        let code = t::seed_strategy(&pool).await;
        let state = t::state(pool.clone());
        state.strategies().reload(&pool).await.unwrap();
        let api = t::seed_open_order(&pool, &code, "API").await;
        let flat = t::seed_open_order(&pool, &code, "FLATTEN").await;
        (pool, code, state, api, flat)
    }

    async fn teardown(pool: &sqlx::PgPool, code: &str) {
        crate::strategies::test_support::drop_orders(pool, code).await;
        crate::strategies::test_support::drop_strategy(pool, code).await;
    }

    /// Against the shared dev DB.
    #[tokio::test]
    #[ignore]
    async fn a_dropped_session_cancels_everything_but_flatten_orders() {
        use crate::strategies::test_support::order_status;
        let _serial = SERIAL.lock().await;
        let (pool, code, state, api, flat) = setup().await;
        let strategy = state.strategies().by_code(&code).unwrap();
        let report = on_session_end(&state, &strategy).await;
        assert_eq!(report.cancelled, 1, "{:?}", report.cancel_failures);
        assert_eq!(order_status(&pool, api).await, "canceled");
        assert_eq!(order_status(&pool, flat).await, "submitted");
        teardown(&pool, &code).await;
    }

    /// Against the shared dev DB.
    #[tokio::test]
    #[ignore]
    async fn kill_halts_cancels_and_logs_out() {
        use crate::strategies::test_support::order_status;
        let _serial = SERIAL.lock().await;
        let (pool, code, state, api, flat) = setup().await;
        let report = kill(&state, &code).await.unwrap();
        assert_eq!(report.cancelled, 2, "{:?}", report.cancel_failures);
        assert_eq!((order_status(&pool, api).await, order_status(&pool, flat).await), ("canceled".into(), "canceled".into()));
        let s = state.strategies().by_code(&code).unwrap();
        assert_eq!((s.state, s.rest_enabled), (crate::domain::orders::risk::TradingState::Halted, false));
        teardown(&pool, &code).await;
    }

    /// Against the shared dev DB.
    #[tokio::test]
    #[ignore]
    async fn boot_recovery_cancels_open_strategy_orders() {
        use crate::strategies::test_support::order_status;
        let _serial = SERIAL.lock().await;
        let (pool, code, state, api, flat) = setup().await;
        boot_recovery(&state).await;
        assert_eq!(order_status(&pool, api).await, "canceled");
        assert_eq!(order_status(&pool, flat).await, "submitted");
        teardown(&pool, &code).await;
    }

    /// A Flatten order at the band's edge passes the band check it is sent through.
    #[test]
    fn a_flatten_price_is_inside_the_band() {
        for mid in [0.0371, 1.2345, 99.99, 61_234.56] {
            for net in [1.0, -1.0] {
                let (_, _, px) = flatten_order(net, mid, 5.0);
                assert!(guard::check_price_band(Some(px), mid, 5.0).is_ok(), "mid {mid} net {net} px {px}");
            }
        }
    }
}
