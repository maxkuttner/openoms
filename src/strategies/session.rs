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
            Inbound::LoggedOn => state.fix_sessions().logged_on(&strategy.code),
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
