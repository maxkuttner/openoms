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

use crate::stream_supervisor::{Session, StreamResult};

/// How long to idle before re-checking when nothing is held. A reconnect re-runs
/// the query, so this is just the poll interval for "did we buy anything yet".
const IDLE_RECHECK_SECS: u64 = 60;

pub struct QuoteFeedSession<F: LiveQuoteFeed> {
    feed: F,
    pool: PgPool,
    out: mpsc::Sender<Quote>,
    position_changed_rx: mpsc::Receiver<()>,
}

impl<F: LiveQuoteFeed> QuoteFeedSession<F> {
    pub fn new(
        feed: F,
        pool: PgPool,
        out: mpsc::Sender<Quote>,
        position_changed_rx: mpsc::Receiver<()>,
    ) -> Self {
        Self { feed, pool, out, position_changed_rx }
    }
}

#[async_trait::async_trait]
impl<F: LiveQuoteFeed> Session for QuoteFeedSession<F> {
    async fn run_once(&mut self) -> StreamResult {
        let known = load_subscribable(&self.pool, self.feed.code()).await?;
        if known.is_empty() {
            info!(source = self.feed.code(), "quote feed: nothing held to subscribe, idling");
            sleep(Duration::from_secs(IDLE_RECHECK_SECS)).await;
            return Ok(());
        }
        info!(source = self.feed.code(), count = known.len(), "quote feed: subscribing held set");

        let (add_tx, mut add_rx) = mpsc::channel::<SymbolAdds>(8);

        // The watcher never ends the session — only the feed decides that. Racing
        // them lets a fill be picked up without waiting for a reconnect.
        tokio::select! {
            r = self.feed.run_session(known.clone(), &self.out, &mut add_rx) => r.map_err(Into::into),
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

/// The held instruments this feed can cover.
///
/// Still the original OPRA-shaped query: held options keyed by the *master*
/// symbol, which works only because Databento's raw symbol happens to be
/// byte-identical to ours. P3 replaces the body with the `instrument_xref`
/// PROVIDER join so the feed's own `external_symbol` drives the subscription and
/// `source_code` selects coverage — that is what makes this genuinely generic, and
/// what lets a feed cover something other than options.
async fn load_subscribable(
    pool: &PgPool,
    _source_code: &str,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows = sqlx::query(
        // position.instrument_id is text holding the numeric instrument.id.
        "SELECT i.symbol, i.id \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.net_qty <> 0 AND i.instrument_class = 'OPTION'",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("symbol"), r.get::<i64, _>("id")))
        .collect())
}
