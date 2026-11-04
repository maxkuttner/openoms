//! Generic driver for any [`dataprovider::LiveQuoteFeed`].
//!
//! Owns everything that is the same regardless of vendor: work out what to
//! subscribe to, hand it to the feed, watch the bus's order lane, and push newly
//! held instruments in mid-session. The feed itself only speaks its wire protocol.
//!
//! Pairing this with [`gateway::stream_supervisor`] means a new vendor is one
//! `impl LiveQuoteFeed` — no reconnect loop, no health wiring, no DB access, no
//! subscription bookkeeping.

use std::collections::HashMap;
use std::sync::Arc;

use dataprovider::{FeedSymbology, Quote, SymbolAdds};
use sqlx::PgPool;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{sleep, Duration};
use tracing::info;

use gateway::connectors::QuoteSource;
use crate::event_bus::OrderLaneEvent;
use gateway::stream_health::StreamHandle;
use gateway::stream_supervisor::{Session, StreamResult};

/// How long to idle before re-checking when nothing is held. A reconnect re-runs
/// the query, so this is just the poll interval for "did we buy anything yet".
const IDLE_RECHECK_SECS: u64 = 60;

pub struct QuoteFeedSession {
    feed: Arc<dyn QuoteSource>,
    pool: PgPool,
    out: mpsc::Sender<Quote>,
    order_rx: broadcast::Receiver<OrderLaneEvent>,
    health: StreamHandle,
}

impl QuoteFeedSession {
    pub fn new(
        feed: Arc<dyn QuoteSource>,
        pool: PgPool,
        out: mpsc::Sender<Quote>,
        order_rx: broadcast::Receiver<OrderLaneEvent>,
        health: StreamHandle,
    ) -> Self {
        Self { feed, pool, out, order_rx, health }
    }

    /// Block until there is something to subscribe to.
    ///
    /// Deliberately does *not* return to the supervisor when nothing is held: an
    /// idle feed is not a closed stream, and reporting it as one would flap the
    /// health badge and log a reconnect warning every minute for a feed behaving
    /// exactly as designed. Waits on the order lane so a fill is picked up at once,
    /// with a timer as the backstop for positions this process didn't see.
    async fn wait_for_subscribable(&mut self) -> Result<HashMap<String, i64>, sqlx::Error> {
        loop {
            let held = load_subscribable(&self.pool, self.feed.as_ref()).await?;
            if !held.is_empty() {
                return Ok(held);
            }
            tokio::select! {
                _ = next_recheck(&mut self.order_rx) => {}
                _ = sleep(Duration::from_secs(IDLE_RECHECK_SECS)) => {}
            }
        }
    }
}

#[async_trait::async_trait]
impl Session for QuoteFeedSession {
    async fn run_once(&mut self) -> StreamResult {
        let known = self.wait_for_subscribable().await?;
        info!(source = self.feed.code(), count = known.len(), "quote feed: subscribing held set");

        let (add_tx, mut add_rx) = mpsc::channel::<SymbolAdds>(8);

        // The watcher never ends the session — only the feed decides that. Racing
        // them lets a fill be picked up without waiting for a reconnect.
        tokio::select! {
            r = self.feed.run_session(known.clone(), &self.out, &mut add_rx, &self.health) => r.map_err(Into::into),
            r = watch_held(&self.pool, self.feed.as_ref(), &mut self.order_rx, &add_tx, known) => r,
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

/// On each position change, re-read the held set and push anything new to the feed.
///
/// Re-reads rather than trusting a payload: the DB is the truth, and a re-read
/// cannot act on a stale event.
async fn watch_held(
    pool: &PgPool,
    feed: &dyn FeedSymbology,
    order_rx: &mut broadcast::Receiver<OrderLaneEvent>,
    add_tx: &mpsc::Sender<SymbolAdds>,
    mut known: HashMap<String, i64>,
) -> StreamResult {
    loop {
        next_recheck(order_rx).await;
        let latest = load_subscribable(pool, feed).await?;
        let adds: SymbolAdds = latest
            .into_iter()
            .filter(|(sym, _)| !known.contains_key(sym))
            .collect();
        if adds.is_empty() {
            continue;
        }
        info!(
            source = feed.code(),
            count = adds.len(),
            "quote feed: subscribing newly-held instruments"
        );
        known.extend(adds.iter().map(|(s, i)| (s.clone(), *i)));
        if add_tx.send(adds).await.is_err() {
            // The feed dropped its receiver: the session is ending anyway.
            // Park rather than return, so the feed's own result is what the
            // supervisor sees.
            std::future::pending::<()>().await;
        }
    }
}

/// The held instruments this feed can price, as `feed_symbol -> instrument_id`.
///
/// Derived, not stored. The feed declares the slice of the catalog it covers
/// ([`FeedSymbology::candidates`]) and how it names it
/// ([`FeedSymbology::to_feed_symbol`]); both are pure, so the mapping is a function
/// of the catalog and cannot go stale or miss instruments seeded later. A held
/// instrument outside `candidates`, or one `to_feed_symbol` declines, is silently
/// absent — that is the "held but this feed can't price it" state, not an error;
/// another feed may. [`crate::preflight`] is what makes it visible.
///
/// The mapping is 1:n (one feed symbol may price instruments on several venues), so
/// a `feed_symbol` collision keeps the last instrument id. In practice held sets
/// don't collide today (one venue per crypto pair); true fan-out to multiple held
/// instruments from one quote is a later change in the mark path.
pub(crate) async fn load_subscribable(
    pool: &PgPool,
    feed: &dyn FeedSymbology,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    // Held only: this is the subscription set, not the catalog. Push the feed's
    // filter down rather than scanning every instrument.
    // position.instrument_id is text holding the numeric instrument.id.
    let mut sql = String::from(
        "SELECT DISTINCT i.id, i.symbol \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.net_qty <> 0 AND i.status = 'ACTIVE'",
    );
    let binds = feed.candidates().push_conditions(&mut sql, 1);

    let mut query = sqlx::query_as::<_, (i64, String)>(&sql);
    for b in &binds {
        query = query.bind(*b);
    }

    Ok(query
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter_map(|(id, symbol)| feed.to_feed_symbol(&symbol).map(|s| (s, id)))
        .collect())
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
}
