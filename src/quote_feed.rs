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
            let held = load_subscribable(&self.pool, self.feed.code(), self.feed.covers()).await?;
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
            r = watch_held(&self.pool, self.feed.code(), self.feed.covers(), &mut self.position_changed_rx, &add_tx, known) => r,
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
    covers: &'static [&'static str],
    doorbell: &mut mpsc::Receiver<()>,
    add_tx: &mpsc::Sender<SymbolAdds>,
    mut known: HashMap<String, i64>,
) -> StreamResult {
    loop {
        match doorbell.recv().await {
            Some(()) => {
                let latest = load_subscribable(pool, source_code, covers).await?;
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

/// The held instruments this feed can cover, as `external_symbol -> instrument_id`.
///
/// Driven by `instrument_xref`, which is the point: the *provider's own* symbol
/// drives the subscription, so a feed is no longer required to use strings that
/// happen to match ours. The previous query read `instrument.symbol` directly and
/// worked only by the coincidence that Databento's raw symbol is byte-identical to
/// our master symbol — a coincidence that holds for OSI options and for nothing
/// else. Binance calls it `SOLUSDT` where we call it `SOL`.
///
/// Coverage is `(source_code, instrument_class)`. Both halves are load-bearing:
/// source_code alone would hand this feed the held SPY *equity*, which is
/// cross-referenced to DATABENTO but not quotable on OPRA.PILLAR.
///
/// An instrument with no xref row for this source is silently absent — that is the
/// "tradable but unmarkable" state, not an error.
async fn load_subscribable(
    pool: &PgPool,
    source_code: &str,
    covers: &[&str],
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let classes: Vec<String> = covers.iter().map(|c| c.to_string()).collect();
    let rows = sqlx::query(
        // position.instrument_id is text holding the numeric instrument.id.
        "SELECT DISTINCT x.external_symbol, i.id \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         JOIN oms.instrument_xref x ON x.instrument_id = i.id \
              AND x.source_type = 'PROVIDER' AND x.source_code = $1 \
         WHERE p.net_qty <> 0 \
           AND i.instrument_class = ANY($2) \
           AND x.external_symbol IS NOT NULL",
    )
    .bind(source_code)
    .bind(&classes)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("external_symbol"), r.get::<i64, _>("id")))
        .collect())
}
