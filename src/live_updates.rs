//! Live updates for the trade cockpit: one WebSocket per open screen, fed from
//! both bus lanes.
//!
//! It sends notices, not data — "orders changed", "positions changed", "these
//! marks moved". The cockpit answers a notice by refetching through the REST
//! endpoints it already uses, so permission filtering stays in one place and a
//! socket reveals nothing a REST call wouldn't. Events are conflated per socket:
//! a burst of fills becomes one notice.

use std::collections::BTreeSet;

use tokio::sync::broadcast;

use crate::event_bus::{MarkUpdate, OrderLaneEvent};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiNotice {
    Orders,
    Positions,
    Marks { instrument_ids: Vec<i64> },
    /// The socket fell behind the order lane: refetch everything.
    Resync,
}

/// What has changed since the last flush.
#[derive(Default)]
pub struct Conflator {
    orders: bool,
    positions: bool,
    resync: bool,
    marks: BTreeSet<i64>,
}

impl Conflator {
    pub fn on_order(&mut self, r: Result<OrderLaneEvent, broadcast::error::RecvError>) {
        match r {
            Ok(OrderLaneEvent::Appended { .. }) => self.orders = true,
            Ok(OrderLaneEvent::PositionChanged { .. }) => self.positions = true,
            Err(broadcast::error::RecvError::Lagged(_)) => self.resync = true,
            Err(broadcast::error::RecvError::Closed) => {}
        }
    }

    pub fn on_mark(&mut self, m: MarkUpdate) {
        self.marks.insert(m.instrument_id);
    }

    /// Pending order/position/resync notices; a resync covers the other two.
    pub fn take_fast(&mut self) -> Vec<UiNotice> {
        let notices = if self.resync {
            vec![UiNotice::Resync]
        } else {
            [(self.orders, UiNotice::Orders), (self.positions, UiNotice::Positions)]
                .into_iter()
                .filter_map(|(pending, n)| pending.then_some(n))
                .collect()
        };
        (self.orders, self.positions, self.resync) = (false, false, false);
        notices
    }

    /// Pending marks as one notice (ids ascending), or nothing.
    pub fn take_marks(&mut self) -> Option<UiNotice> {
        if self.marks.is_empty() {
            return None;
        }
        let instrument_ids = std::mem::take(&mut self.marks).into_iter().collect();
        Some(UiNotice::Marks { instrument_ids })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;
    use uuid::Uuid;

    fn appended() -> OrderLaneEvent {
        OrderLaneEvent::Appended { order_id: Uuid::nil(), global_position: 1, event_type: "order_filled".into() }
    }

    fn position_changed() -> OrderLaneEvent {
        OrderLaneEvent::PositionChanged { portfolio_id: Uuid::nil(), instrument_id: "1".into() }
    }

    fn mark(instrument_id: i64) -> MarkUpdate {
        MarkUpdate { instrument_id, bid: 1.0, ask: 2.0 }
    }

    #[test]
    fn burst_conflates_to_one_notice_each() {
        let mut c = Conflator::default();
        for _ in 0..200 {
            c.on_order(Ok(appended()));
            c.on_order(Ok(position_changed()));
        }
        assert_eq!(c.take_fast(), vec![UiNotice::Orders, UiNotice::Positions]);
        assert_eq!(c.take_fast(), vec![]);
    }

    #[test]
    fn order_lag_becomes_resync() {
        let mut c = Conflator::default();
        c.on_order(Ok(appended()));
        c.on_order(Err(RecvError::Lagged(5)));
        assert_eq!(c.take_fast(), vec![UiNotice::Resync]);
    }

    #[test]
    fn marks_collect_distinct_ids() {
        let mut c = Conflator::default();
        for id in [3, 1, 3] {
            c.on_mark(mark(id));
        }
        assert_eq!(c.take_marks(), Some(UiNotice::Marks { instrument_ids: vec![1, 3] }));
        assert_eq!(c.take_marks(), None);
    }

    #[test]
    fn notices_serialize_with_type_tag() {
        assert_eq!(serde_json::to_string(&UiNotice::Orders).unwrap(), r#"{"type":"orders"}"#);
        assert_eq!(
            serde_json::to_string(&UiNotice::Marks { instrument_ids: vec![1, 3] }).unwrap(),
            r#"{"type":"marks","instrument_ids":[1,3]}"#
        );
    }
}
