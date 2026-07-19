//! Generic driver for any [`LiveQuoteFeed`].
//!
//! Owns everything that is the same regardless of vendor: work out what to
//! subscribe to, hand it to the feed, watch the position doorbell, and push newly
//! held instruments in mid-session. The feed itself only speaks its wire protocol.
//!
//! Pairing this with [`crate::stream_supervisor`] means a new vendor is one
//! `impl LiveQuoteFeed` — no reconnect loop, no health wiring, no DB access, no
//! subscription bookkeeping.

use std::collections::HashMap;

use dataprovider::{LiveQuoteFeed, Quote, SymbolAdds};
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::stream_health::StreamHandle;
use crate::stream_supervisor::{Session, StreamResult};

/// How long to idle before re-checking when nothing is held. A reconnect re-runs
/// the query, so this is just the poll interval for "did we buy anything yet".
const IDLE_RECHECK_SECS: u64 = 60;

pub struct QuoteFeedSession<F: LiveQuoteFeed> {
    feed: F,
    pool: PgPool,
    out: mpsc::Sender<Quote>,
    position_changed_rx: mpsc::Receiver<()>,
    health: StreamHandle,
}

impl<F: LiveQuoteFeed> QuoteFeedSession<F> {
    pub fn new(
        feed: F,
        pool: PgPool,
        out: mpsc::Sender<Quote>,
        position_changed_rx: mpsc::Receiver<()>,
        health: StreamHandle,
    ) -> Self {
        Self { feed, pool, out, position_changed_rx, health }
    }

    /// Block until there is something to subscribe to.
    ///
    /// Deliberately does *not* return to the supervisor when nothing is held: an
    /// idle feed is not a closed stream, and reporting it as one would flap the
    /// health badge and log a reconnect warning every minute for a feed behaving
    /// exactly as designed. Waits on the doorbell so a fill is picked up at once,
    /// with a timer as the backstop for positions this process didn't see.
    async fn wait_for_subscribable(&mut self) -> Result<HashMap<String, i64>, sqlx::Error> {
        loop {
            let held = load_subscribable(&self.pool, self.feed.code()).await?;
            if !held.is_empty() {
                return Ok(held);
            }
            tokio::select! {
                _ = self.position_changed_rx.recv() => {}
                _ = sleep(Duration::from_secs(IDLE_RECHECK_SECS)) => {}
            }
        }
    }
}

#[async_trait::async_trait]
impl<F: LiveQuoteFeed> Session for QuoteFeedSession<F> {
    async fn run_once(&mut self) -> StreamResult {
        let known = self.wait_for_subscribable().await?;
        info!(source = self.feed.code(), count = known.len(), "quote feed: subscribing held set");

        let (add_tx, mut add_rx) = mpsc::channel::<SymbolAdds>(8);

        // The watcher never ends the session — only the feed decides that. Racing
        // them lets a fill be picked up without waiting for a reconnect.
        tokio::select! {
            r = self.feed.run_session(known.clone(), &self.out, &mut add_rx, &self.health) => r.map_err(Into::into),
            r = watch_held(&self.pool, self.feed.code(), &mut self.position_changed_rx, &add_tx, known) => r,
        }
    }
}

/// On each doorbell ring, re-read the held set and push anything new to the feed.
///
/// Re-reads rather than trusting a payload: the DB is the truth, and a signal that
/// carries no data cannot carry stale data.
async fn watch_held(
    pool: &PgPool,
    source_code: &'static str,
    doorbell: &mut mpsc::Receiver<()>,
    add_tx: &mpsc::Sender<SymbolAdds>,
    mut known: HashMap<String, i64>,
) -> StreamResult {
    loop {
        match doorbell.recv().await {
            Some(()) => {
                let latest = load_subscribable(pool, source_code).await?;
                let adds: SymbolAdds = latest
                    .into_iter()
                    .filter(|(sym, _)| !known.contains_key(sym))
                    .collect();
                if adds.is_empty() {
                    continue;
                }
                info!(
                    source = source_code,
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
            // Doorbell closed (only if every fill stream is gone). Never end the
            // session over it — a feed with no doorbell still streams fine.
            None => std::future::pending::<()>().await,
        }
    }
}

/// The held instruments this feed can price, as `feed_symbol -> instrument_id`.
///
/// Driven by `feed_instrument`: the feed's *own* symbol drives the subscription,
/// and the mapping is the single source of what this feed covers. An instrument
/// with no `feed_instrument` row for this feed is silently absent — that is the
/// "held but this feed can't price it" state, not an error; another feed may.
///
/// `feed_instrument` is 1:n (one feed symbol may price instruments on several
/// venues), so a `feed_symbol` collision keeps the last instrument id. In practice
/// held sets don't collide today (one venue per crypto pair); true fan-out to
/// multiple held instruments from one quote is a later change in the mark path.
async fn load_subscribable(
    pool: &PgPool,
    feed_code: &str,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows = sqlx::query(
        // position.instrument_id is text holding the numeric instrument.id.
        "SELECT DISTINCT fi.feed_symbol, i.id \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         JOIN feed_instrument fi ON fi.instrument_id = i.id \
              AND fi.feed_code = $1 AND fi.is_active \
         WHERE p.net_qty <> 0",
    )
    .bind(feed_code)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("feed_symbol"), r.get::<i64, _>("id")))
        .collect())
}
