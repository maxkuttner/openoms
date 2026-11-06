//! In-process event bus: one place where what happened inside the OMS is published,
//! for anything in the process to subscribe to.
//!
//! Two lanes, because they behave differently:
//! - order lane: committed order events and position changes. Published only after
//!   the transaction commits, in append order *per publishing task*. There is no
//!   global order: two tasks committing concurrently can publish in either order,
//!   and `global_position` is assigned at insert, not commit, so it can differ
//!   from commit order too. A consumer resuming by `global_position` must
//!   tolerate gaps.
//! - price lane: accepted marks. Latest wins; a lagging subscriber skips ahead and a
//!   producer never waits on it.
//!
//! Publishing never blocks and never fails: with no subscribers the event is dropped.

use tokio::sync::broadcast;
use uuid::Uuid;

use crate::event_store::{AppendResult, NewOrderEvent};

pub const ORDER_LANE_CAPACITY: usize = 1024;
pub const PRICE_LANE_CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum OrderLaneEvent {
    /// One committed order event (published after commit; see module doc on ordering).
    Appended { order_id: Uuid, global_position: i64, event_type: String },
    /// A committed fill changed a position.
    PositionChanged { portfolio_id: Uuid, instrument_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkUpdate {
    pub instrument_id: i64,
    pub bid: f64,
    pub ask: f64,
}

impl From<&dataprovider::Quote> for MarkUpdate {
    fn from(q: &dataprovider::Quote) -> Self {
        Self { instrument_id: q.instrument_id, bid: q.bid, ask: q.ask }
    }
}

#[derive(Clone)]
pub struct EventBus {
    order: broadcast::Sender<OrderLaneEvent>,
    price: broadcast::Sender<MarkUpdate>,
}

impl EventBus {
    pub fn new() -> Self {
        let (order, _) = broadcast::channel(ORDER_LANE_CAPACITY);
        let (price, _) = broadcast::channel(PRICE_LANE_CAPACITY);
        Self { order, price }
    }

    // send only errs when nobody is subscribed, which is not a failure
    pub fn publish_order(&self, ev: OrderLaneEvent) {
        let _ = self.order.send(ev);
    }

    pub fn publish_mark(&self, m: MarkUpdate) {
        let _ = self.price.send(m);
    }

    pub fn subscribe_order(&self) -> broadcast::Receiver<OrderLaneEvent> {
        self.order.subscribe()
    }

    // no production subscriber yet: the first is the UI websocket (phase 3)
    #[allow(dead_code)]
    pub fn subscribe_price(&self) -> broadcast::Receiver<MarkUpdate> {
        self.price.subscribe()
    }
}

/// Publish one `Appended` per committed event. Call only after the append's
/// transaction has committed.
pub fn publish_appended(bus: &EventBus, order_id: Uuid, events: &[NewOrderEvent], result: &AppendResult) {
    for (event, &global_position) in events.iter().zip(&result.global_positions) {
        bus.publish_order(OrderLaneEvent::Appended {
            order_id,
            global_position,
            event_type: event.event_type.clone(),
        });
    }
}

/// An allocation transfer moved a position between portfolios without any order
/// event: announce every portfolio it touched, once each. Call after commit.
pub fn publish_transfer(bus: &EventBus, from_portfolio: Uuid, to_portfolios: &[Uuid], instrument_id: &str) {
    let mut seen = vec![from_portfolio];
    for &p in to_portfolios {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    for portfolio_id in seen {
        bus.publish_order(OrderLaneEvent::PositionChanged { portfolio_id, instrument_id: instrument_id.to_string() });
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tokio::sync::broadcast::error::TryRecvError;
    use uuid::Uuid;

    fn appended(global_position: i64) -> OrderLaneEvent {
        OrderLaneEvent::Appended {
            order_id: Uuid::nil(),
            global_position,
            event_type: "OrderFilled".into(),
        }
    }

    fn mark(bid: f64) -> MarkUpdate {
        MarkUpdate { instrument_id: 7, bid, ask: bid + 1.0 }
    }

    #[test]
    fn publish_without_subscribers_is_a_no_op() {
        let bus = EventBus::new();
        bus.publish_order(appended(1));
        bus.publish_mark(mark(1.0));
    }

    #[test]
    fn order_lane_delivers_in_publish_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();
        for p in 1..=3 {
            bus.publish_order(appended(p));
        }
        for p in 1..=3 {
            assert_eq!(rx.try_recv().unwrap(), appended(p));
        }
    }

    #[test]
    fn price_lane_lagging_subscriber_skips_to_newest() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe_price();
        for i in 0..=PRICE_LANE_CAPACITY {
            bus.publish_mark(mark(i as f64));
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(_))));
        let mut last = None;
        while let Ok(m) = rx.try_recv() {
            last = Some(m);
        }
        assert_eq!(last.unwrap().bid, PRICE_LANE_CAPACITY as f64);
    }

    #[test]
    fn price_burst_does_not_touch_order_lane() {
        let bus = EventBus::new();
        let mut order_rx = bus.subscribe_order();
        let _price_rx = bus.subscribe_price();
        for i in 0..5000 {
            bus.publish_mark(mark(i as f64));
        }
        assert!(matches!(order_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn mark_update_copies_quote_prices() {
        let q = dataprovider::Quote {
            instrument_id: 42,
            bid: 100.5,
            ask: 101.0,
            bid_size: 3,
            ask_size: 4,
            ts_recv: Utc::now(),
            source_code: "BINANCE",
        };
        assert_eq!(MarkUpdate::from(&q), MarkUpdate { instrument_id: 42, bid: 100.5, ask: 101.0 });
    }

    #[test]
    fn transfer_publishes_position_changed_for_each_portfolio() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();

        publish_transfer(&bus, a, &[b, c, b], "42");

        for portfolio_id in [a, b, c] {
            assert_eq!(
                rx.try_recv().unwrap(),
                OrderLaneEvent::PositionChanged { portfolio_id, instrument_id: "42".into() }
            );
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn publish_appended_pairs_types_with_positions() {
        let ev = |t: &str| NewOrderEvent {
            event_id: Uuid::new_v4().to_string(),
            event_type: t.to_string(),
            actor: "test".to_string(),
            payload: serde_json::json!({}),
            correlation_id: None,
            causation_id: None,
            schema_version: 0,
        };
        let order_id = Uuid::new_v4();
        let bus = EventBus::new();
        let mut rx = bus.subscribe_order();
        let result = AppendResult { last_version: 2, appended: 2, global_positions: vec![10, 11] };

        publish_appended(&bus, order_id, &[ev("order_partially_filled"), ev("order_filled")], &result);

        let expect = |global_position, t: &str| OrderLaneEvent::Appended {
            order_id,
            global_position,
            event_type: t.to_string(),
        };
        assert_eq!(rx.try_recv().unwrap(), expect(10, "order_partially_filled"));
        assert_eq!(rx.try_recv().unwrap(), expect(11, "order_filled"));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}
