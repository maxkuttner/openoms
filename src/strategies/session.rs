//! Strategies' FIX sessions, on the OMS side of the acceptor.
//!
//! The acceptor (`gateway::fix_acceptor`) speaks FIX; this module decides who
//! may log on (from the registry snapshot), turns a strategy's requests into
//! calls on the one order path, and answers with reports. Execution reports
//! for its orders follow from the order lane.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use gateway::fix_acceptor::{
    AcceptorConfig, AcceptorHandle, ClientReport, ClientRequest, ExecType, Inbound, LogonCheck, OrdStatus, OrdType,
    SessionKey, Side, Tif,
};

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::config::FixAcceptorSettings;
use crate::domain::orders::risk::TradingState;
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};
use crate::handlers::SubmitOrderRequest;
use crate::order_entry::{self, Origin};

use super::{verify_fix_password, RegistrySnapshot, Strategy};

pub fn session_key(s: &Strategy) -> SessionKey {
    SessionKey { sender_comp_id: s.sender_comp_id.clone(), target_comp_id: s.target_comp_id.clone() }
}

/// Whether `strategy` may log on with `password`, given whether it already
/// has a live session. `Err` is the reason, logged and kept as the strategy's
/// `last_logon_error`.
pub fn logon_decision(strategy: Option<&Strategy>, password: Option<&str>, live: bool) -> Result<(), String> {
    let s = strategy.ok_or("unknown CompIDs")?;
    let hash = s.fix_password_hash.as_deref().ok_or("no FIX password set")?;
    if !password.is_some_and(|p| verify_fix_password(p, hash)) {
        return Err("wrong password".into());
    }
    if s.state == TradingState::Halted {
        return Err("strategy is halted".into());
    }
    if live {
        return Err("a session is already logged on".into());
    }
    Ok(())
}

/// The acceptor's logon check, from the registry snapshot.
struct StrategyLogon {
    strategies: RegistrySnapshot,
    live: Arc<Mutex<BTreeSet<String>>>,
    pool: PgPool,
    /// QuickFIX calls back on its own thread; writes go through the runtime.
    runtime: tokio::runtime::Handle,
}

impl LogonCheck for StrategyLogon {
    fn check(&self, key: &SessionKey, password: Option<&str>) -> Result<(), String> {
        let strategy = self.strategies.by_comp_ids(&key.sender_comp_id, &key.target_comp_id);
        let live = strategy.as_ref().is_some_and(|s| self.live.lock().unwrap().contains(&s.code));
        let decision = logon_decision(strategy.as_ref(), password, live);
        if let Some(s) = strategy {
            let (pool, error) = (self.pool.clone(), decision.clone().err());
            self.runtime.spawn(async move {
                let _ = sqlx::query("UPDATE strategy SET last_logon_error = $2 WHERE code = $1")
                    .bind(&s.code)
                    .bind(error)
                    .execute(&pool)
                    .await;
            });
        }
        decision
    }
}

/// The running acceptor, if one is configured, and which strategies are
/// logged on. Cheap to clone; every clone sees the same acceptor.
#[derive(Clone, Default)]
pub struct FixSessions {
    acceptor: Arc<Mutex<Option<AcceptorHandle>>>,
    live: Arc<Mutex<BTreeSet<String>>>,
    settings: Arc<Mutex<Option<FixAcceptorSettings>>>,
    reporting: Arc<std::sync::atomic::AtomicBool>,
    delivery: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl FixSessions {
    pub fn is_live(&self, code: &str) -> bool {
        self.live.lock().unwrap().contains(code)
    }

    /// Start the acceptor with a session per registered strategy. A no-op
    /// when `settings` is `None`.
    pub fn start(&self, state: &AppState, settings: Option<FixAcceptorSettings>) -> Result<(), String> {
        *self.settings.lock().unwrap() = settings.clone();
        let Some(settings) = settings else {
            info!("FIX acceptor: not configured");
            return Ok(());
        };
        let sessions = state
            .strategies()
            .all()
            .iter()
            .map(|s| (session_key(s), s.allowed_addresses.clone()))
            .collect();
        let cfg = AcceptorConfig {
            port: settings.port,
            certificate: settings.certificate.into(),
            private_key: settings.private_key.into(),
            sessions,
        };
        let logon = StrategyLogon {
            strategies: state.strategies().clone(),
            live: self.live.clone(),
            pool: state.pool().clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let (handle, inbound) = AcceptorHandle::start(cfg, Arc::new(logon))?;
        *self.acceptor.lock().unwrap() = Some(handle);
        tokio::spawn(run(state.clone(), inbound));
        if !self.reporting.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tokio::spawn(report_loop(state.clone()));
        }
        Ok(())
    }

    /// Stop the acceptor and start it again from the registry as it is now;
    /// returns how many sessions were logged out.
    pub fn restart(&self, state: &AppState) -> Result<usize, String> {
        let old = self.acceptor.lock().unwrap().take();
        let logged_out = old.map(AcceptorHandle::stop).unwrap_or(0);
        let settings = self.settings.lock().unwrap().clone();
        self.start(state, settings)?;
        Ok(logged_out)
    }

    pub fn send(&self, key: &SessionKey, report: ClientReport) {
        if let Some(acceptor) = self.acceptor.lock().unwrap().as_ref() {
            if let Err(e) = acceptor.send(key, report) {
                warn!(session = ?key, error = %e, "FIX: report not sent");
            }
        }
    }

    pub fn logout(&self, key: &SessionKey, reason: &str) {
        if let Some(acceptor) = self.acceptor.lock().unwrap().as_ref() {
            acceptor.logout(key, reason);
        }
    }

    fn delivery_lock(&self, code: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.delivery.lock().unwrap().entry(code.to_string()).or_default().clone()
    }

    fn logged_on(&self, code: &str) {
        self.live.lock().unwrap().insert(code.to_string());
    }

    /// Whether `code` had a live session (a refused Logon never had one).
    fn logged_out(&self, code: &str) -> bool {
        self.live.lock().unwrap().remove(code)
    }
}

/// Handle what the acceptor reports until it stops.
async fn run(state: AppState, mut inbound: mpsc::UnboundedReceiver<(SessionKey, Inbound)>) {
    while let Some((key, event)) = inbound.recv().await {
        let Some(strategy) = state.strategies().by_comp_ids(&key.sender_comp_id, &key.target_comp_id) else {
            warn!(session = ?key, "FIX: event for a session no strategy owns");
            continue;
        };
        match event {
            Inbound::LoggedOn => {
                state.fix_sessions().logged_on(&strategy.code);
                let state = state.clone();
                tokio::spawn(async move { replay(&state, &strategy).await });
            }
            Inbound::LoggedOut => {
                state.fix_sessions().logged_out(&strategy.code);
            }
            Inbound::Request(request) => {
                // Each request on its own task: one leg never waits on another.
                let state = state.clone();
                tokio::spawn(async move {
                    if let Some(report) = handle(&state, &strategy, request).await {
                        state.fix_sessions().send(&key, report);
                    }
                });
            }
        }
    }
}

fn auth(strategy: &Strategy) -> AuthContext {
    AuthContext { principal_id: strategy.principal_id, principal_code: format!("strategy:{}", strategy.code) }
}

fn rejected(cl_ord_id: &str, symbol: &str, side: Side, qty: f64, text: String) -> ClientReport {
    ClientReport::Exec {
        cl_ord_id: cl_ord_id.to_string(),
        order_id: "NONE".into(),
        exec_id: Uuid::new_v4().to_string(),
        exec_type: ExecType::Rejected,
        ord_status: OrdStatus::Rejected,
        symbol: symbol.to_string(),
        side,
        order_qty: qty,
        cum_qty: 0.0,
        leaves_qty: 0.0,
        avg_px: 0.0,
        last_qty: None,
        last_px: None,
        text: Some(text),
    }
}

/// The OMS order a strategy's ClOrdID names.
async fn order_by_cl_ord_id(pool: &PgPool, strategy: &Strategy, cl_ord_id: &str) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT order_id FROM order_state WHERE portfolio_id = $1 AND fix_cl_ord_id = $2")
        .bind(strategy.portfolio_id)
        .bind(cl_ord_id)
        .fetch_optional(pool)
        .await
}

/// The answer to one request, if it gets one now. A placed order or a
/// requested cancel answers later, from the order lane.
async fn handle(state: &AppState, strategy: &Strategy, request: ClientRequest) -> Option<ClientReport> {
    let pool = state.pool();
    match request {
        ClientRequest::NewOrder { cl_ord_id, symbol, ex_destination, side, qty, ord_type, price, tif } => {
            match order_by_cl_ord_id(pool, strategy, &cl_ord_id).await {
                Ok(None) => {}
                Ok(Some(_)) => return Some(rejected(&cl_ord_id, &symbol, side, qty, format!("duplicate ClOrdID {cl_ord_id}"))),
                Err(e) => return Some(rejected(&cl_ord_id, &symbol, side, qty, format!("could not check ClOrdID: {e}"))),
            }
            let req = SubmitOrderRequest {
                order_id: Uuid::new_v4().to_string(),
                client_order_id: cl_ord_id.clone(),
                portfolio_id: strategy.portfolio_id.to_string(),
                account_id: None,
                instrument_id: None,
                symbol: Some(symbol.clone()),
                venue: Some(ex_destination),
                side: match side {
                    Side::Buy => OrderSide::Buy,
                    Side::Sell => OrderSide::Sell,
                },
                order_type: match ord_type {
                    OrdType::Market => OrderType::Market,
                    OrdType::Limit => OrderType::Limit,
                },
                time_in_force: match tif {
                    Tif::Day => TimeInForce::Day,
                    Tif::Gtc => TimeInForce::Gtc,
                    Tif::Ioc => TimeInForce::Ioc,
                },
                limit_price: price,
                quantity: qty,
            };
            match order_entry::submit(state, &auth(strategy), req, Origin::Fix { cl_ord_id: cl_ord_id.clone() }).await {
                Ok(_) => None,
                Err(e) => Some(rejected(&cl_ord_id, &symbol, side, qty, e.message)),
            }
        }
        ClientRequest::Cancel { cl_ord_id, orig_cl_ord_id } => {
            let cancel_reject = |text: String| ClientReport::CancelReject { cl_ord_id: cl_ord_id.clone(), orig_cl_ord_id: orig_cl_ord_id.clone(), text };
            let order_id = match order_by_cl_ord_id(pool, strategy, &orig_cl_ord_id).await {
                Ok(Some(id)) => id,
                Ok(None) => return Some(cancel_reject(format!("unknown order {orig_cl_ord_id}"))),
                Err(e) => return Some(cancel_reject(format!("could not find order: {e}"))),
            };
            match order_entry::cancel(state, &auth(strategy), order_id, Some(format!("FIX cancel {cl_ord_id}"))).await {
                Ok(_) => None,
                Err(e) => Some(cancel_reject(e.message)),
            }
        }
        ClientRequest::Status { cl_ord_id } => match order_by_cl_ord_id(pool, strategy, &cl_ord_id).await {
            Ok(Some(order_id)) => match status_report(pool, order_id, ExecType::OrderStatus, Uuid::new_v4().to_string(), None).await {
                Ok(Some(report)) => Some(report),
                Ok(None) => None,
                Err(e) => {
                    error!(error = %e, "FIX: status report failed");
                    None
                }
            },
            Ok(None) => Some(rejected(&cl_ord_id, "", Side::Buy, 0.0, format!("unknown order {cl_ord_id}"))),
            Err(e) => Some(rejected(&cl_ord_id, "", Side::Buy, 0.0, format!("could not find order: {e}"))),
        },
        ClientRequest::SecurityList { req_id } => match tradeable(pool, strategy).await {
            Ok(securities) => Some(ClientReport::SecurityList { req_id, securities }),
            Err(e) => {
                error!(error = %e, "FIX: security list failed");
                Some(ClientReport::SecurityList { req_id, securities: Vec::new() })
            }
        },
    }
}

/// The FIX execution type and order status an order event reports as; `None`
/// for events a strategy is not told about (submitted, suspended, …).
pub fn exec_for(event_type: &str) -> Option<(ExecType, OrdStatus)> {
    match event_type {
        "order_routed" => Some((ExecType::New, OrdStatus::New)),
        "order_partially_filled" => Some((ExecType::Trade, OrdStatus::PartiallyFilled)),
        "order_filled" => Some((ExecType::Trade, OrdStatus::Filled)),
        "order_canceled" => Some((ExecType::Canceled, OrdStatus::Canceled)),
        "order_rejected" => Some((ExecType::Rejected, OrdStatus::Rejected)),
        "order_expired" => Some((ExecType::Expired, OrdStatus::Expired)),
        _ => None,
    }
}

/// Of `events` (global_position, event_type), the ones to report after `cursor`.
pub fn reports_to_replay(cursor: i64, events: &[(i64, &str)]) -> Vec<i64> {
    events.iter().filter(|(pos, t)| *pos > cursor && exec_for(t).is_some()).map(|(pos, _)| *pos).collect()
}

/// (fill_qty, fill_price) wherever a fill event's payload carries them.
pub fn fill_of(payload: &serde_json::Value) -> Option<(f64, f64)> {
    match payload {
        serde_json::Value::Object(map) => match (map.get("fill_qty").and_then(|v| v.as_f64()), map.get("fill_price").and_then(|v| v.as_f64())) {
            (Some(q), Some(p)) => Some((q, p)),
            _ => map.values().find_map(fill_of),
        },
        _ => None,
    }
}

/// A rejection's or cancel's reason, wherever its payload carries one.
fn reason_of(payload: &serde_json::Value) -> Option<String> {
    match payload {
        serde_json::Value::Object(map) => match map.get("reason").and_then(|v| v.as_str()) {
            Some(r) => Some(r.to_string()),
            None => map.values().find_map(reason_of),
        },
        _ => None,
    }
}

/// The report one stored order event makes, for a FIX order; `None` for any
/// other order or event. Quantities are the order's as they stand now; the
/// type and status are the event's.
async fn report_for_event(pool: &PgPool, order_id: Uuid, global_position: i64, event_type: &str) -> Result<Option<ClientReport>, sqlx::Error> {
    let Some((exec_type, status)) = exec_for(event_type) else { return Ok(None) };
    let payload: serde_json::Value = sqlx::query_scalar("SELECT payload_json FROM order_event WHERE global_position = $1")
        .bind(global_position)
        .fetch_optional(pool)
        .await?
        .unwrap_or_default();
    let last = if exec_type == ExecType::Trade { fill_of(&payload) } else { None };
    let report = status_report(pool, order_id, exec_type, global_position.to_string(), last).await?;
    Ok(report.map(|r| match r {
        ClientReport::Exec { cl_ord_id, order_id, exec_id, exec_type, symbol, side, order_qty, cum_qty, leaves_qty, avg_px, last_qty, last_px, .. } => {
            ClientReport::Exec {
                cl_ord_id, order_id, exec_id, exec_type, ord_status: status, symbol, side, order_qty, cum_qty, leaves_qty,
                avg_px, last_qty, last_px, text: reason_of(&payload),
            }
        }
        other => other,
    }))
}

/// Move the strategy's cursor to `global_position` and send the report, once:
/// a position the cursor already passed is not sent again.
async fn deliver(state: &AppState, strategy: &Strategy, global_position: i64, report: ClientReport) -> Result<(), sqlx::Error> {
    let claimed = sqlx::query("UPDATE strategy SET fix_report_position = $2 WHERE code = $1 AND fix_report_position < $2")
        .bind(&strategy.code)
        .bind(global_position)
        .execute(state.pool())
        .await?
        .rows_affected()
        == 1;
    if claimed {
        state.fix_sessions().send(&session_key(strategy), report);
    }
    Ok(())
}

/// Everything on the strategy's FIX orders since its cursor, in order. The
/// only way reports reach a strategy, live or after a logon, and serialized
/// per strategy, so each goes out once and in order.
async fn replay(state: &AppState, strategy: &Strategy) {
    let lock = state.fix_sessions().delivery_lock(&strategy.code);
    let _held = lock.lock().await;
    let result: Result<(), sqlx::Error> = async {
        let cursor: i64 = sqlx::query_scalar("SELECT fix_report_position FROM strategy WHERE code = $1")
            .bind(&strategy.code)
            .fetch_one(state.pool())
            .await?;
        let rows = sqlx::query_as::<_, (i64, Uuid, String)>(
            "SELECT e.global_position, e.order_id, e.event_type FROM order_event e \
             JOIN order_state o ON o.order_id = e.order_id \
             WHERE o.portfolio_id = $1 AND o.origin = 'FIX' AND e.global_position > $2 \
             ORDER BY e.global_position",
        )
        .bind(strategy.portfolio_id)
        .bind(cursor)
        .fetch_all(state.pool())
        .await?;
        let events: Vec<(i64, &str)> = rows.iter().map(|(p, _, t)| (*p, t.as_str())).collect();
        let due = reports_to_replay(cursor, &events);
        for (pos, order_id, event_type) in rows.iter().filter(|(p, _, _)| due.contains(p)) {
            if let Some(report) = report_for_event(state.pool(), *order_id, *pos, event_type).await? {
                deliver(state, strategy, *pos, report).await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        error!(strategy = %strategy.code, error = %e, "FIX: replay failed");
    }
}

/// Report every order event on a FIX order to its strategy, while it is
/// logged on. A lagged lane replays from each live strategy's cursor rather
/// than dropping what it skipped.
async fn report_loop(state: AppState) {
    use crate::event_bus::OrderLaneEvent;
    use tokio::sync::broadcast::error::RecvError;
    let mut lane = state.event_bus().subscribe_order();
    loop {
        match lane.recv().await {
            Ok(OrderLaneEvent::Appended { order_id, global_position: _, event_type }) => {
                if exec_for(&event_type).is_none() {
                    continue;
                }
                let portfolio: Option<Uuid> = sqlx::query_scalar("SELECT portfolio_id FROM order_state WHERE order_id = $1 AND origin = 'FIX'")
                    .bind(order_id)
                    .fetch_optional(state.pool())
                    .await
                    .unwrap_or(None);
                let Some(strategy) = portfolio.and_then(|p| state.strategies().by_portfolio_id(p)) else { continue };
                if !state.fix_sessions().is_live(&strategy.code) {
                    continue; // replayed at its next logon
                }
                replay(&state, &strategy).await;
            }
            Ok(OrderLaneEvent::PositionChanged { .. }) => {}
            Err(RecvError::Lagged(skipped)) => {
                warn!(skipped, "FIX: order lane lagged; replaying for every live strategy");
                for strategy in state.strategies().all().iter().filter(|s| state.fix_sessions().is_live(&s.code)) {
                    replay(&state, strategy).await;
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Every instrument the strategy may trade: (broker code, broker symbol, OMS id).
pub async fn tradeable(pool: &PgPool, strategy: &Strategy) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    let brokers: Vec<String> = strategy.connections.keys().cloned().collect();
    sqlx::query_as::<_, (String, String, i64)>(
        "SELECT bi.broker_code, bi.broker_symbol, bi.instrument_id FROM broker_instrument bi \
         JOIN instrument i ON i.id = bi.instrument_id \
         WHERE bi.broker_code = ANY($1) AND bi.is_tradeable = true AND i.status = 'ACTIVE' \
         ORDER BY bi.broker_code, bi.broker_symbol",
    )
    .bind(brokers)
    .fetch_all(pool)
    .await
}

pub(crate) fn ord_status(status: &str) -> OrdStatus {
    match status {
        "partially_filled" => OrdStatus::PartiallyFilled,
        "filled" => OrdStatus::Filled,
        "canceled" => OrdStatus::Canceled,
        "rejected" => OrdStatus::Rejected,
        "expired" => OrdStatus::Expired,
        _ => OrdStatus::New,
    }
}

/// An execution report for a FIX order as it stands now; `None` for an order
/// that did not come over FIX.
pub(crate) async fn status_report(
    pool: &PgPool,
    order_id: Uuid,
    exec_type: ExecType,
    exec_id: String,
    last: Option<(f64, f64)>,
) -> Result<Option<ClientReport>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT o.fix_cl_ord_id, o.side, o.status, \
                o.original_qty::double precision AS original_qty, o.cum_qty::double precision AS cum_qty, \
                o.leaves_qty::double precision AS leaves_qty, o.avg_px::double precision AS avg_px, \
                COALESCE(bi.broker_symbol, '') AS symbol \
         FROM order_state o \
         LEFT JOIN broker_connection bc ON bc.code = o.broker_connection_code \
         LEFT JOIN broker_instrument bi ON bi.instrument_id::text = o.instrument_id AND bi.broker_code = bc.broker_code \
         WHERE o.order_id = $1 AND o.origin = 'FIX'",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let status: String = row.get("status");
    Ok(Some(ClientReport::Exec {
        cl_ord_id: row.get::<Option<String>, _>("fix_cl_ord_id").unwrap_or_default(),
        order_id: order_id.to_string(),
        exec_id,
        exec_type,
        ord_status: ord_status(&status),
        symbol: row.get("symbol"),
        side: if row.get::<String, _>("side") == "sell" { Side::Sell } else { Side::Buy },
        order_qty: row.get("original_qty"),
        cum_qty: row.get("cum_qty"),
        leaves_qty: row.get("leaves_qty"),
        avg_px: row.get::<Option<f64>, _>("avg_px").unwrap_or(0.0),
        last_qty: last.map(|(q, _)| q),
        last_px: last.map(|(_, p)| p),
        text: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::{hash_fix_password, tests::sample};

    #[test]
    fn logon_refuses_unknown_comp_ids_a_wrong_password_a_halted_strategy_and_a_second_session() {
        let mut s = sample("s", "S", "OMS");
        assert_eq!(logon_decision(None, Some("pw"), false).unwrap_err(), "unknown CompIDs");
        assert_eq!(logon_decision(Some(&s), Some("pw"), false).unwrap_err(), "no FIX password set");
        s.fix_password_hash = Some(hash_fix_password("pw"));
        assert_eq!(logon_decision(Some(&s), Some("nope"), false).unwrap_err(), "wrong password");
        assert_eq!(logon_decision(Some(&s), None, false).unwrap_err(), "wrong password");
        assert!(logon_decision(Some(&s), Some("pw"), false).is_ok());
        assert_eq!(logon_decision(Some(&s), Some("pw"), true).unwrap_err(), "a session is already logged on");
        s.state = TradingState::Halted;
        assert_eq!(logon_decision(Some(&s), Some("pw"), false).unwrap_err(), "strategy is halted");
    }

    #[test]
    fn routed_fill_cancel_reject_map_to_fix_exec_and_status() {
        assert_eq!(exec_for("order_routed"), Some((ExecType::New, OrdStatus::New)));
        assert_eq!(exec_for("order_partially_filled"), Some((ExecType::Trade, OrdStatus::PartiallyFilled)));
        assert_eq!(exec_for("order_filled"), Some((ExecType::Trade, OrdStatus::Filled)));
        assert_eq!(exec_for("order_canceled"), Some((ExecType::Canceled, OrdStatus::Canceled)));
        assert_eq!(exec_for("order_rejected"), Some((ExecType::Rejected, OrdStatus::Rejected)));
        assert_eq!(exec_for("order_expired"), Some((ExecType::Expired, OrdStatus::Expired)));
        assert_eq!(exec_for("order_submitted"), None);
    }

    #[test]
    fn logon_replays_reports_past_the_cursor_once() {
        let events = [(9, "order_routed"), (11, "order_canceled"), (12, "order_submitted")];
        assert_eq!(reports_to_replay(10, &events), vec![11]);
        assert!(reports_to_replay(12, &events).is_empty());
    }

    #[test]
    fn a_fill_payload_gives_last_qty_and_price() {
        let payload = serde_json::json!({ "event_id": "e", "payload": { "fill_qty": 2.0, "fill_price": 101.5, "cum_qty": 2.0 } });
        assert_eq!(fill_of(&payload), Some((2.0, 101.5)));
        assert_eq!(fill_of(&serde_json::json!({ "reason": "x" })), None);
    }

    /// Against the shared dev DB: a fill on a FIX order is reported and moves
    /// the strategy's cursor; a second replay sends nothing again.
    #[tokio::test]
    #[ignore]
    async fn a_fill_on_a_fix_order_reaches_the_session_and_advances_the_cursor() {
        use crate::strategies::test_support;
        let pool = test_support::dev_pool().await;
        let code = test_support::seed_strategy(&pool).await;
        let state = test_support::state(pool.clone());
        state.strategies().reload(&pool).await.unwrap();
        let strategy = state.strategies().by_code(&code).unwrap();
        let (account, connection): (Uuid, String) =
            sqlx::query_as("SELECT id, broker_connection_code FROM account LIMIT 1").fetch_one(&pool).await.unwrap();
        let order_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, broker_connection_code, \
                instrument_id, side, order_type, time_in_force, original_qty, leaves_qty, cum_qty, avg_px, status, version, origin, fix_cl_ord_id) \
             VALUES ($1, 'c1', $2, $3, $4, $5, '1', 'buy', 'limit', 'gtc', 1, 0, 1, 100, 'filled', 1, 'FIX', 'c1')",
        )
        .bind(order_id).bind(strategy.principal_id).bind(strategy.portfolio_id).bind(account).bind(&connection)
        .execute(&pool).await.unwrap();
        let pos: i64 = sqlx::query_scalar(
            "INSERT INTO order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, 1, $2, 'order_filled', '{\"fill_qty\": 1, \"fill_price\": 100}') RETURNING global_position",
        )
        .bind(order_id).bind(Uuid::new_v4())
        .fetch_one(&pool).await.unwrap();

        replay(&state, &strategy).await;
        let cursor: i64 = sqlx::query_scalar("SELECT fix_report_position FROM strategy WHERE code = $1").bind(&code).fetch_one(&pool).await.unwrap();
        assert_eq!(cursor, pos);

        sqlx::query("DELETE FROM order_event WHERE order_id = $1").bind(order_id).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM order_state WHERE order_id = $1").bind(order_id).execute(&pool).await.unwrap();
        test_support::drop_strategy(&pool, &code).await;
    }

    /// Against the shared dev DB (read-only): the FIX Security List and the
    /// REST list come from the same query, scoped to the strategy's brokers.
    #[tokio::test]
    #[ignore]
    async fn rest_and_fix_security_lists_agree() {
        let pool = crate::strategies::test_support::dev_pool().await;
        let mut s = sample("s", "S", "OMS");
        s.connections.insert("BINANCE".into(), Uuid::new_v4());
        let list = tradeable(&pool, &s).await.unwrap();
        assert!(!list.is_empty());
        assert!(list.iter().all(|(venue, _, _)| venue == "BINANCE"));
    }
}
