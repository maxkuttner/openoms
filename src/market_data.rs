//! What is held, told to the market data gateway.
//!
//! The core's half of market data: it reads the held set from the database and
//! keeps the gateway's subscriptions in step with it — at start, on each position
//! change on the bus's order lane, and at least every minute. Which feed covers
//! what, and the sessions themselves, are the gateway's (`gateway::md`).

use std::collections::BTreeSet;

use dataprovider::InstrumentRef;
use gateway::md::MdHandle;
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::event_bus::OrderLaneEvent;

/// This loop's owner name on the market data gateway.
const HELD: &str = "held";

/// The backstop re-read interval, for positions this process didn't see change.
const IDLE_RECHECK_SECS: u64 = 60;

/// Every held instrument, with what a feed needs to decide whether it covers it.
pub async fn held_instruments(pool: &PgPool) -> Result<Vec<InstrumentRef>, sqlx::Error> {
    // position.instrument_id is text holding the numeric instrument.id.
    let rows = sqlx::query_as::<_, (i64, Option<String>, Option<String>, Option<String>, Option<String>)>(
        "SELECT DISTINCT i.id, i.symbol, i.instrument_class, i.asset_class, i.venue \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.net_qty <> 0 AND i.status = 'ACTIVE'",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, symbol, instrument_class, asset_class, venue)| {
            Some(InstrumentRef { id, symbol: symbol?, instrument_class, asset_class, venue })
        })
        .collect())
}

/// What to tell the gateway, given what it was last told: (to subscribe, to unsubscribe).
pub fn diff(sent: &BTreeSet<i64>, held: &[InstrumentRef]) -> (Vec<InstrumentRef>, Vec<i64>) {
    let now: BTreeSet<i64> = held.iter().map(|i| i.id).collect();
    let subscribe = held.iter().filter(|i| !sent.contains(&i.id)).cloned().collect();
    let unsubscribe = sent.difference(&now).copied().collect();
    (subscribe, unsubscribe)
}

/// Keep the gateway's subscriptions equal to the held set, for the life of the
/// process. A failed read keeps the last set.
pub async fn run(pool: PgPool, md: MdHandle, mut order_rx: broadcast::Receiver<OrderLaneEvent>) -> ! {
    let mut sent = BTreeSet::new();
    loop {
        match held_instruments(&pool).await {
            Ok(held) => {
                let (subscribe, unsubscribe) = diff(&sent, &held);
                if !subscribe.is_empty() {
                    info!(count = subscribe.len(), "market data: subscribing held instruments");
                    sent.extend(subscribe.iter().map(|i| i.id));
                    md.subscribe(HELD, subscribe);
                }
                if !unsubscribe.is_empty() {
                    info!(count = unsubscribe.len(), "market data: unsubscribing instruments no longer held");
                    for id in &unsubscribe {
                        sent.remove(id);
                    }
                    md.unsubscribe(HELD, &unsubscribe);
                }
            }
            Err(e) => warn!(error = %e, "market data: could not read held instruments, keeping the last set"),
        }
        tokio::select! {
            _ = next_recheck(&mut order_rx) => {}
            _ = sleep(Duration::from_secs(IDLE_RECHECK_SECS)) => {}
        }
    }
}

/// What an order-lane receive means for a feed's held set.
#[derive(Debug, PartialEq)]
enum Recheck {
    Yes,
    No,
    Closed,
}

/// A position change means the held set may have grown. So does a lag: the
/// skipped events may have included one, and re-reading is cheap. Other order
/// events leave positions alone.
fn needs_recheck(r: &Result<OrderLaneEvent, broadcast::error::RecvError>) -> Recheck {
    match r {
        Ok(OrderLaneEvent::PositionChanged { .. }) => Recheck::Yes,
        Ok(OrderLaneEvent::Appended { .. }) => Recheck::No,
        Err(broadcast::error::RecvError::Lagged(_)) => Recheck::Yes,
        Err(broadcast::error::RecvError::Closed) => Recheck::Closed,
    }
}

/// Resolve on the next order-lane event that warrants a re-read. The lane only
/// closes when the bus is gone; never end the session over it — a feed with no
/// order lane still streams fine — so park instead.
///
/// Drains whatever is already queued before resolving: one re-read covers a
/// whole burst of fills, so a sweep of partials costs one query, not one each.
async fn next_recheck(order_rx: &mut broadcast::Receiver<OrderLaneEvent>) {
    loop {
        match needs_recheck(&order_rx.recv().await) {
            Recheck::Yes => {
                while !matches!(
                    order_rx.try_recv(),
                    Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed)
                ) {}
                return;
            }
            Recheck::No => {}
            Recheck::Closed => std::future::pending::<()>().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;
    use uuid::Uuid;

    #[test]
    fn position_changed_means_recheck() {
        let ev = OrderLaneEvent::PositionChanged { portfolio_id: Uuid::nil(), instrument_id: "1".into() };
        assert_eq!(needs_recheck(&Ok(ev)), Recheck::Yes);
    }

    #[test]
    fn appended_alone_is_not_a_recheck() {
        let ev = OrderLaneEvent::Appended {
            order_id: Uuid::nil(),
            global_position: 1,
            event_type: "order_routed".into(),
        };
        assert_eq!(needs_recheck(&Ok(ev)), Recheck::No);
    }

    #[test]
    fn lagged_order_lane_means_recheck() {
        assert_eq!(needs_recheck(&Err(RecvError::Lagged(3))), Recheck::Yes);
    }

    /// A sweep of partial fills must cost one held-set re-read, not one per fill.
    #[tokio::test]
    async fn next_recheck_coalesces_a_burst() {
        let bus = crate::event_bus::EventBus::new();
        let mut rx = bus.subscribe_order();
        for _ in 0..3 {
            bus.publish_order(OrderLaneEvent::PositionChanged { portfolio_id: Uuid::nil(), instrument_id: "1".into() });
        }

        next_recheck(&mut rx).await;

        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }

    #[test]
    fn closed_order_lane_parks() {
        assert_eq!(needs_recheck(&Err(RecvError::Closed)), Recheck::Closed);
    }

    fn held(id: i64) -> InstrumentRef {
        InstrumentRef { id, symbol: format!("S{id}"), instrument_class: None, asset_class: None, venue: None }
    }

    #[test]
    fn diff_subscribes_new_and_unsubscribes_gone() {
        let sent = BTreeSet::from([1, 2]);
        assert_eq!(diff(&sent, &[held(2), held(3)]), (vec![held(3)], vec![1]));
    }
}
