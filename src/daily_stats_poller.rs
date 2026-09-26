//! Periodically refreshes `DailyStatsStore` from every registered broker
//! adapter's own snapshot/24hr-stats endpoint (`BrokerAdapter::daily_stats`),
//! for whatever instruments are "interesting" right now: watched, or held.
//! Independent of any HTTP request — a request for `/marks` never blocks on
//! a broker call.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use tracing::{info, warn};

use crate::adapters::BrokerRegistry;
use crate::daily_stats::DailyStatsStore;

const POLL_INTERVAL_SECS: u64 = 300;

/// The union of two instrument-id lists, deduplicated. A pure merge step,
/// factored out so the "did we dedupe/union correctly" question doesn't need
/// a database to answer.
pub fn interesting_instrument_ids(watchlisted: &[i64], held: &[i64]) -> Vec<i64> {
    let mut set: HashSet<i64> = HashSet::new();
    set.extend(watchlisted.iter().copied());
    set.extend(held.iter().copied());
    set.into_iter().collect()
}

async fn watchlisted_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT instrument_id FROM watchlist_item")
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().filter_map(|s| s.parse().ok()).collect())
}

async fn held_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT instrument_id FROM position WHERE net_qty <> 0",
    )
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().filter_map(|s| s.parse().ok()).collect())
}

/// instrument.symbol for a set of ids, as `(id, symbol)` — the poller needs
/// symbols to call each adapter with; the stores it writes to are keyed by
/// id (matching `MarkStore`).
async fn symbols_for(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (i64, String)>(
        "SELECT id, symbol FROM instrument WHERE id = ANY($1) AND status = 'ACTIVE'",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
}

async fn poll_once(pool: &PgPool, registry: &BrokerRegistry, stats: &DailyStatsStore) {
    let watchlisted = watchlisted_instrument_ids(pool).await.unwrap_or_else(|e| {
        warn!(error = %e, "daily stats: failed to load watchlist instruments");
        Vec::new()
    });
    let held = held_instrument_ids(pool).await.unwrap_or_else(|e| {
        warn!(error = %e, "daily stats: failed to load held instruments");
        Vec::new()
    });
    let ids = interesting_instrument_ids(&watchlisted, &held);
    let by_id = match symbols_for(pool, &ids).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "daily stats: failed to resolve symbols, skipping this pass");
            return;
        }
    };
    if by_id.is_empty() {
        return;
    }
    let symbol_to_id: std::collections::HashMap<String, i64> =
        by_id.iter().map(|(id, sym)| (sym.clone(), *id)).collect();
    let symbols: Vec<String> = by_id.iter().map(|(_, sym)| sym.clone()).collect();

    for ((broker_code, environment), adapter) in registry.iter() {
        match adapter.daily_stats(&symbols).await {
            Ok(results) => {
                let mut updated = 0;
                for (symbol, prev_close) in results {
                    if let Some(&id) = symbol_to_id.get(&symbol) {
                        stats.set(id, prev_close);
                        updated += 1;
                    }
                }
                if updated > 0 {
                    info!(broker_code, environment, updated, "daily stats: refreshed");
                }
            }
            Err(crate::adapters::BrokerError::NotConfigured(_)) => {
                // This adapter doesn't support it. Not an error — most won't.
            }
            Err(e) => {
                warn!(broker_code, environment, error = %e, "daily stats: adapter call failed, others continue");
            }
        }
    }
}

pub async fn run(pool: PgPool, registry: Arc<ArcSwap<BrokerRegistry>>, stats: DailyStatsStore) {
    let mut interval = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    loop {
        interval.tick().await;
        poll_once(&pool, &registry.load(), &stats).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // interesting_instrument_ids is a pure function over two id lists — the
    // union, deduplicated. The SQL that PRODUCES those two lists (watchlist
    // rows, held positions) is exercised by Task 8's/Task 1's own DB-backed
    // tests; this only tests the merge logic, which is where a bug ("forgot
    // to dedupe", "used intersection instead of union") would actually hide.
    #[test]
    fn unions_and_dedupes_watchlist_and_held() {
        let watchlist = vec![1i64, 2, 3];
        let held = vec![2i64, 3, 4];
        let mut result = interesting_instrument_ids(&watchlist, &held);
        result.sort();
        assert_eq!(result, vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert_eq!(interesting_instrument_ids(&[], &[]), Vec::<i64>::new());
    }
}
