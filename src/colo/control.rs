//! Kill, Enable and the alarms for co-located strategies. Kill acts at the
//! venue with the OMS's own key and keeps acting while the strategy keeps
//! trading; it also tells the strategy over its control link.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{error, warn};

use colo_link::proto::{Position, ToStrategy};
use gateway::adapter::BrokerAdapter;
use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::contract::FollowEvent;

use super::ColoHub;
use crate::app_state::AppState;
use crate::domain::orders::risk::TradingState;
use crate::strategies::control::ControlReport;
use crate::strategies::Strategy;

/// How many orders the OMS cancels per second per strategy while fighting
/// (within Binance's order-rate limits).
pub const FIGHT_CANCELS_PER_SEC: usize = 10;

/// Limits must be acknowledged this soon after they change.
pub const ACK_LIMITS_WITHIN: Duration = Duration::from_secs(10);

/// A trailing-second cancel budget.
#[derive(Default)]
pub struct CancelRate(VecDeque<Instant>);

impl CancelRate {
    pub fn allow(&mut self, now: Instant) -> bool {
        while self.0.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            self.0.pop_front();
        }
        if self.0.len() >= FIGHT_CANCELS_PER_SEC {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

/// The order to cancel on sight: any new order of a killed strategy.
/// A kill (when it was sent, whether it was acknowledged) still waiting for
/// its acknowledgement past 5 s.
pub fn kill_ack_late(kill: Option<(Instant, bool)>, now: Instant) -> bool {
    kill.is_some_and(|(sent, acked)| !acked && now.saturating_duration_since(sent) > ACK_KILL_WITHIN)
}

pub fn fight(killed: bool, event: &FollowEvent) -> Option<(String, String)> {
    match event {
        FollowEvent::Opened { venue_order_id, symbol, .. } if killed => Some((venue_order_id.clone(), symbol.clone())),
        _ => None,
    }
}

/// A kill should be acknowledged this soon.
pub const ACK_KILL_WITHIN: Duration = Duration::from_secs(5);

/// How long after its link went down (or the OMS started) a strategy may
/// still trade: its dead man's switch running out, plus a reconnect.
pub fn no_link_grace(dead_man_secs: u32) -> Duration {
    Duration::from_secs(u64::from(dead_man_secs) + 2)
}

/// Why a trading strategy is unmonitored, if it is: no link for longer than
/// `grace` (`down_for` is `None` while connected), or the current limits not
/// acknowledged within 10 s of changing.
pub fn unmonitored(down_for: Option<Duration>, grace: Duration, acked: i64, current: i64, changed_at: Instant, now: Instant) -> Option<String> {
    if let Some(down_for) = down_for {
        return (down_for > grace).then(|| "unmonitored: no control link".into());
    }
    if acked < current && now.saturating_duration_since(changed_at) > ACK_LIMITS_WITHIN {
        return Some(format!("unmonitored: limits version {current} not acknowledged"));
    }
    None
}

/// The strategy's booked positions, by venue symbol.
pub async fn positions(state: &AppState, strategy: &Strategy) -> Vec<Position> {
    sqlx::query_as::<_, (String, f64)>(
        "SELECT bi.broker_symbol, p.net_qty::double precision FROM position p \
         JOIN broker_instrument bi ON bi.instrument_id = p.instrument_id::bigint AND bi.broker_code = 'BINANCE' \
         WHERE p.portfolio_id = $1",
    )
    .bind(strategy.portfolio_id)
    .fetch_all(state.pool())
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(symbol, qty)| Position { symbol, qty })
    .collect()
}

pub async fn push_positions(state: &AppState, hub: &ColoHub, strategy: &Strategy) {
    hub.push(&strategy.code, ToStrategy::Positions { positions: positions(state, strategy).await });
}

#[derive(Debug, PartialEq)]
pub enum PositionsPush {
    Portfolio(uuid::Uuid),
    /// The lane lagged: every co-located strategy may have missed a change.
    Every,
    Nothing,
}

pub fn positions_push(event: &Result<crate::event_bus::OrderLaneEvent, tokio::sync::broadcast::error::RecvError>) -> PositionsPush {
    use crate::event_bus::OrderLaneEvent;
    use tokio::sync::broadcast::error::RecvError;
    match event {
        Ok(OrderLaneEvent::PositionChanged { portfolio_id, .. }) => PositionsPush::Portfolio(*portfolio_id),
        Err(RecvError::Lagged(_)) => PositionsPush::Every,
        _ => PositionsPush::Nothing,
    }
}

/// Send a co-located strategy its booked positions whenever they change,
/// however they changed (its fills, a transfer, a correction).
pub async fn positions_loop(state: AppState) {
    let mut lane = state.event_bus().subscribe_order();
    loop {
        let event = lane.recv().await;
        if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
            return;
        }
        let targets: Vec<Strategy> = match positions_push(&event) {
            PositionsPush::Portfolio(p) => state.strategies().by_portfolio_id(p).into_iter().collect(),
            PositionsPush::Every => state.strategies().all().to_vec(),
            PositionsPush::Nothing => continue,
        };
        for strategy in targets.iter().filter(|s| s.mode == crate::strategies::Mode::Colocated) {
            push_positions(&state, state.colo(), strategy).await;
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Cancel {
    Done,
    /// Filled or cancelled already (the strategy often cancels its own first).
    AlreadyGone,
    Failed(String),
}

/// Binance answers -2011 to a cancel of an order that is no longer open.
pub fn cancel_outcome(result: Result<(), String>) -> Cancel {
    match result {
        Ok(()) => Cancel::Done,
        Err(e) if e.contains("\"code\":-2011") => Cancel::AlreadyGone,
        Err(e) => Cancel::Failed(e),
    }
}

async fn cancel(adapter: &BinanceAdapter, venue_order_id: &str, symbol: &str) -> Cancel {
    cancel_outcome(adapter.cancel_order(venue_order_id, symbol).await.map_err(|e| e.to_string()))
}

/// Tell a co-located strategy it is killed: the first thing Kill does,
/// before any database work, so the strategy stops as soon as it can.
pub fn announce_kill(hub: &ColoHub, mode: Option<crate::strategies::Mode>, code: &str) {
    if mode == Some(crate::strategies::Mode::Colocated) {
        hub.push(code, ToStrategy::Kill);
    }
}

/// Kill a co-located strategy (already told, and set HALTED): cancel every
/// open order on its account with the OMS's key, without waiting for it.
pub async fn kill(state: &AppState, strategy: &Strategy) -> ControlReport {
    let hub = state.colo();
    let mut report = ControlReport::default();
    let Some(adapter) = hub.adapter(&strategy.code) else {
        report.cancel_failures.push("no follow key: cannot cancel at the venue".into());
        return report;
    };
    match adapter.open_orders().await {
        Ok(open) => {
            for order in open {
                match cancel(&adapter, &order.venue_order_id, &order.symbol).await {
                    Cancel::Done => report.cancelled += 1,
                    Cancel::AlreadyGone => {}
                    Cancel::Failed(e) => report.cancel_failures.push(format!("{}: {e}", order.venue_order_id)),
                }
            }
        }
        Err(e) => report.cancel_failures.push(format!("could not list open orders: {e}")),
    }
    report
}

/// How often a killed strategy's account is swept for orders the fight missed
/// (budget spent, a dropped stream).
pub const SWEEP_EVERY: Duration = Duration::from_secs(2);

pub fn to_sweep(strategies: &[Strategy]) -> Vec<String> {
    strategies
        .iter()
        .filter(|s| s.mode == crate::strategies::Mode::Colocated && s.state == TradingState::Halted)
        .map(|s| s.code.clone())
        .collect()
}

/// While a strategy is killed, cancel whatever is still open on its account,
/// within the fight's cancel budget, until Enable.
pub async fn sweep_loop(state: AppState) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    loop {
        tick.tick().await;
        let hub = state.colo();
        for code in to_sweep(&state.strategies().all()) {
            let Some(adapter) = hub.adapter(&code) else { continue };
            let open = match adapter.open_orders().await {
                Ok(open) => open,
                Err(e) => {
                    warn!(strategy = %code, error = %e, "co-located: sweep could not list open orders");
                    continue;
                }
            };
            if open.is_empty() {
                continue;
            }
            hub.fighting.lock().unwrap().insert(code.clone(), Instant::now());
            for order in open {
                if !hub.cancel_rates.lock().unwrap().entry(code.clone()).or_default().allow(Instant::now()) {
                    break;
                }
                if let Cancel::Failed(e) = cancel(&adapter, &order.venue_order_id, &order.symbol).await {
                    warn!(strategy = %code, venue_order_id = %order.venue_order_id, error = %e, "co-located: sweep could not cancel");
                }
            }
        }
    }
}

pub async fn enable(state: &AppState, strategy: &Strategy) {
    state.colo().fighting.lock().unwrap().remove(&strategy.code);
    state.colo().push(&strategy.code, ToStrategy::Resume);
}

/// Per follow event: fight a killed strategy's new orders, and keep the
/// alarms current.
pub async fn on_event(state: &AppState, hub: &ColoHub, strategy: &Strategy, adapter: &std::sync::Arc<BinanceAdapter>, event: &FollowEvent) {
    let killed = strategy.state == TradingState::Halted;
    if let Some((venue_order_id, symbol)) = fight(killed, event) {
        hub.fighting.lock().unwrap().insert(strategy.code.clone(), Instant::now());
        let allowed = hub.cancel_rates.lock().unwrap().entry(strategy.code.clone()).or_default().allow(Instant::now());
        if allowed {
            let (adapter, code) = (adapter.clone(), strategy.code.clone());
            tokio::spawn(async move {
                if let Cancel::Failed(e) = cancel(&adapter, &venue_order_id, &symbol).await {
                    warn!(strategy = %code, %venue_order_id, error = %e, "co-located: could not cancel a killed strategy's order");
                }
            });
        } else {
            warn!(strategy = %strategy.code, %venue_order_id, "co-located: cancel budget spent; order left for the next sweep");
        }
        return;
    }
    if killed || !matches!(event, FollowEvent::Opened { .. } | FollowEvent::Fill(_)) {
        return;
    }
    let link = hub.link_status(&strategy.code);
    let changed_at = hub.limits_changed.lock().unwrap().get(&strategy.code).copied().unwrap_or_else(Instant::now);
    let mut alarm = unmonitored(
        hub.link_down_for(&strategy.code),
        no_link_grace(strategy.dead_man_secs),
        link.acked_version,
        strategy.limits_version,
        changed_at,
        Instant::now(),
    );
    if alarm.is_none() {
        let placed = hub.runtimes.lock().unwrap().get(&strategy.code).map_or(0, |r| r.placed_within(Duration::from_secs(1)));
        if strategy.max_orders_per_sec.is_some_and(|cap| placed > cap as usize) {
            alarm = Some(format!("order rate: {placed} orders in the last second"));
        }
    }
    match alarm {
        Some(reason) => {
            error!(strategy = %strategy.code, %reason, "co-located: strategy is trading unmonitored");
            hub.alarms.lock().unwrap().insert(strategy.code.clone(), reason);
            if strategy.kill_when_unmonitored && reason_is_unmonitored(hub, &strategy.code) {
                if let Err(e) = crate::strategies::control::kill(state, &strategy.code).await {
                    error!(strategy = %strategy.code, error = %e.message, "co-located: kill when unmonitored failed");
                }
            }
        }
        None => {
            hub.alarms.lock().unwrap().remove(&strategy.code);
        }
    }
}

fn reason_is_unmonitored(hub: &ColoHub, code: &str) -> bool {
    hub.alarms.lock().unwrap().get(code).is_some_and(|r| r.starts_with("unmonitored"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gateway::contract::FollowEvent;

    use super::*;

    fn opened(id: &str) -> FollowEvent {
        FollowEvent::Opened { venue_order_id: id.into(), client_order_id: "mm1-q-1".into(), symbol: "BTCUSDT".into(), buy: true, qty: 1.0, price: 100.0 }
    }

    #[test]
    fn positions_go_down_the_link_on_every_position_change() {
        use crate::event_bus::OrderLaneEvent;
        use tokio::sync::broadcast::error::RecvError;
        let p = uuid::Uuid::new_v4();
        assert_eq!(positions_push(&Ok(OrderLaneEvent::PositionChanged { portfolio_id: p, instrument_id: "1".into() })), PositionsPush::Portfolio(p));
        assert_eq!(positions_push(&Ok(OrderLaneEvent::Appended { order_id: p, global_position: 1, event_type: "Filled".into() })), PositionsPush::Nothing);
        assert_eq!(positions_push(&Err(RecvError::Lagged(3))), PositionsPush::Every);
    }

    #[test]
    fn only_killed_colocated_strategies_are_swept() {
        use crate::strategies::{tests::sample, Mode};
        let mut killed = sample("mm1", "A", "B");
        killed.mode = Mode::Colocated;
        killed.state = TradingState::Halted;
        let mut active = killed.clone();
        active.code = "mm2".into();
        active.state = TradingState::Active;
        let mut via_oms = killed.clone();
        via_oms.code = "fix1".into();
        via_oms.mode = Mode::ViaOms;
        assert_eq!(to_sweep(&[killed, active, via_oms]), ["mm1"]);
    }

    #[test]
    fn an_order_already_gone_is_not_a_failed_cancel() {
        assert_eq!(cancel_outcome(Ok(())), Cancel::Done);
        assert_eq!(cancel_outcome(Err(r#"broker rejected: {"code":-2011,"msg":"Unknown order sent."}"#.into())), Cancel::AlreadyGone);
        assert!(matches!(cancel_outcome(Err("timeout".into())), Cancel::Failed(_)));
    }

    #[test]
    fn a_kill_reaches_the_link_before_anything_else() {
        use crate::strategies::Mode;
        let hub = ColoHub::default();
        let (_conn, mut rx) = hub.connect("mm1");
        announce_kill(&hub, Some(Mode::ViaOms), "mm1");
        assert!(rx.try_recv().is_err(), "a strategy trading through the OMS has no link");
        announce_kill(&hub, Some(Mode::Colocated), "mm1");
        assert_eq!(rx.try_recv().unwrap(), ToStrategy::Kill);
    }

    #[test]
    fn a_killed_strategys_new_order_is_cancelled_on_sight() {
        assert_eq!(fight(true, &opened("9")), Some(("9".to_string(), "BTCUSDT".to_string())));
        assert_eq!(fight(false, &opened("9")), None);
        assert_eq!(fight(true, &FollowEvent::Connected), None);
    }

    #[test]
    fn stale_limits_raise_the_alarm_after_10s() {
        let t0 = Instant::now();
        let grace = Duration::from_secs(7);
        assert_eq!(unmonitored(None, grace, 6, 7, t0, t0 + Duration::from_secs(9)), None);
        assert!(unmonitored(None, grace, 6, 7, t0, t0 + Duration::from_secs(11)).is_some());
        assert_eq!(unmonitored(None, grace, 7, 7, t0, t0 + Duration::from_secs(60)), None);
    }

    #[test]
    fn no_link_is_an_alarm_only_once_the_strategy_should_have_stopped() {
        let t0 = Instant::now();
        let grace = Duration::from_secs(7);
        assert_eq!(unmonitored(Some(Duration::from_secs(6)), grace, 7, 7, t0, t0), None, "its dead man's switch is still running out");
        assert_eq!(unmonitored(Some(Duration::from_secs(8)), grace, 7, 7, t0, t0), Some("unmonitored: no control link".to_string()));
    }

    #[test]
    fn a_kill_not_acknowledged_within_5s_is_flagged() {
        let t0 = Instant::now();
        assert!(!kill_ack_late(Some((t0, false)), t0 + Duration::from_secs(4)));
        assert!(kill_ack_late(Some((t0, false)), t0 + Duration::from_secs(6)));
        assert!(!kill_ack_late(Some((t0, true)), t0 + Duration::from_secs(60)));
        assert!(!kill_ack_late(None, t0));
    }

    #[test]
    fn cancels_while_fighting_stay_within_the_rate() {
        let t0 = Instant::now();
        let mut bucket = CancelRate::default();
        for _ in 0..FIGHT_CANCELS_PER_SEC {
            assert!(bucket.allow(t0));
        }
        assert!(!bucket.allow(t0 + Duration::from_millis(500)));
        assert!(bucket.allow(t0 + Duration::from_millis(1001)));
    }
}
