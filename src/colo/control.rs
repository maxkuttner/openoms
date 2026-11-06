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
pub fn fight(killed: bool, event: &FollowEvent) -> Option<(String, String)> {
    match event {
        FollowEvent::Opened { venue_order_id, symbol, .. } if killed => Some((venue_order_id.clone(), symbol.clone())),
        _ => None,
    }
}

/// Why a trading strategy is unmonitored, if it is: no link, or the current
/// limits not acknowledged within 10 s of changing.
pub fn unmonitored(connected: bool, acked: i64, current: i64, changed_at: Instant, now: Instant) -> Option<String> {
    if !connected {
        return Some("unmonitored: no control link".into());
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

async fn cancel(adapter: &BinanceAdapter, venue_order_id: &str, symbol: &str) -> Result<(), String> {
    adapter.cancel_order(venue_order_id, symbol).await.map_err(|e| e.to_string())
}

/// Kill a co-located strategy (already set HALTED): tell it, and cancel every
/// open order on its account with the OMS's key, without waiting for it.
pub async fn kill(state: &AppState, strategy: &Strategy) -> ControlReport {
    let hub = state.colo();
    hub.push(&strategy.code, ToStrategy::Kill);
    let mut report = ControlReport::default();
    let Some(adapter) = hub.adapter(&strategy.code) else {
        report.cancel_failures.push("no follow key: cannot cancel at the venue".into());
        return report;
    };
    match adapter.open_orders().await {
        Ok(open) => {
            for order in open {
                match cancel(&adapter, &order.venue_order_id, &order.symbol).await {
                    Ok(()) => report.cancelled += 1,
                    Err(e) => report.cancel_failures.push(format!("{}: {e}", order.venue_order_id)),
                }
            }
        }
        Err(e) => report.cancel_failures.push(format!("could not list open orders: {e}")),
    }
    report
}

pub async fn enable(state: &AppState, strategy: &Strategy) {
    state.colo().fighting.lock().unwrap().remove(&strategy.code);
    state.colo().push(&strategy.code, ToStrategy::Resume);
}

/// Per follow event: fight a killed strategy's new orders, and keep the
/// alarms current.
pub async fn on_event(state: &AppState, hub: &ColoHub, strategy: &Strategy, adapter: &BinanceAdapter, event: &FollowEvent) {
    let killed = strategy.state == TradingState::Halted;
    if let Some((venue_order_id, symbol)) = fight(killed, event) {
        hub.fighting.lock().unwrap().insert(strategy.code.clone(), Instant::now());
        let allowed = hub.cancel_rates.lock().unwrap().entry(strategy.code.clone()).or_default().allow(Instant::now());
        if allowed {
            if let Err(e) = cancel(adapter, &venue_order_id, &symbol).await {
                warn!(strategy = %strategy.code, %venue_order_id, error = %e, "co-located: could not cancel a killed strategy's order");
            }
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
    let mut alarm = unmonitored(link.connected, link.acked_version, strategy.limits_version, changed_at, Instant::now());
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
    fn a_killed_strategys_new_order_is_cancelled_on_sight() {
        assert_eq!(fight(true, &opened("9")), Some(("9".to_string(), "BTCUSDT".to_string())));
        assert_eq!(fight(false, &opened("9")), None);
        assert_eq!(fight(true, &FollowEvent::Connected), None);
    }

    #[test]
    fn stale_limits_raise_the_alarm_after_10s() {
        let t0 = Instant::now();
        assert_eq!(unmonitored(true, 6, 7, t0, t0 + Duration::from_secs(9)), None);
        assert!(unmonitored(true, 6, 7, t0, t0 + Duration::from_secs(11)).is_some());
        assert_eq!(unmonitored(true, 7, 7, t0, t0 + Duration::from_secs(60)), None);
        assert_eq!(unmonitored(false, 7, 7, t0, t0), Some("unmonitored: no control link".to_string()));
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
