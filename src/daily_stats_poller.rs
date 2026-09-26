//! Periodically refreshes `DailyStatsStore` from every registered broker
//! adapter's own snapshot/24hr-stats endpoint (`BrokerAdapter::daily_stats`),
//! for whatever instruments are "interesting" right now: watched, or held.
//! Independent of any HTTP request — a request for `/marks` never blocks on
//! a broker call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use sqlx::PgPool;
use tracing::{info, warn};

use crate::adapters::BrokerRegistry;
use crate::daily_stats::DailyStatsStore;

const POLL_INTERVAL_SECS: u64 = 300;

/// Crypto venue codes seeded outside the ISO 10383 MIC registry (see
/// `db/scripts/seed_crypto_venues.sql`) — everything else in `instrument.venue`
/// is an equity MIC.
const CRYPTO_VENUES: &[&str] = &["BINANCE", "BYBIT"];

/// Which venues a broker_code's adapter can actually be asked about.
///
/// Binance's `GET /api/v3/ticker/24hr?symbols=[...]` rejects the WHOLE request
/// with `{"code":-1121,"msg":"Invalid symbol."}` if even one symbol in the
/// batch isn't a Binance symbol (live-verified) — so a Binance adapter must
/// only ever be called with Binance symbols, never the full cross-venue set.
///
/// Pure and explicit rather than routed through a registry lookup: there is no
/// existing "which venue does this broker serve" concept to reuse (
/// `dataprovider::FeedSymbology::candidates` is the market-data-feed
/// counterpart of this question, not the broker-adapter one — `BrokerAdapter`
/// carries no such method), and the mapping itself is small and stable enough
/// that a table lookup would be more indirection, not less.
fn broker_serves_venue(broker_code: &str, venue: &str) -> bool {
    match broker_code {
        "BINANCE" => venue == "BINANCE",
        "ALPACA" => !CRYPTO_VENUES.contains(&venue),
        // Any other/future broker_code (e.g. IBKR): not wired for daily
        // stats yet, so it serves nothing here rather than guessing.
        _ => false,
    }
}

/// When more than one environment is registered for the same broker_code
/// (e.g. `("BINANCE","PAPER")` and `("BINANCE","LIVE")` both configured), only
/// one may be asked about that broker's symbols — asking both would have
/// whichever answers second silently overwrite the other's price in
/// `DailyStatsStore` (testnet's price could win over a live-priced
/// instrument). `LIVE` is preferred deterministically; if no `LIVE` entry
/// exists, the lexicographically-first remaining environment name wins, so
/// the choice never depends on the registry's HashMap iteration order.
///
/// Pure over the list of registered `(broker_code, environment)` pairs, so
/// this is unit-testable without a registry or a database.
pub fn select_one_environment_per_broker(registered: &[(String, String)]) -> Vec<(String, String)> {
    let mut by_broker: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (broker_code, environment) in registered {
        by_broker
            .entry(broker_code.clone())
            .and_modify(|current| {
                if environment == "LIVE" || (current != "LIVE" && environment < current) {
                    *current = environment.clone();
                }
            })
            .or_insert_with(|| environment.clone());
    }
    by_broker.into_iter().collect()
}

/// `symbol -> instrument_id` for one broker's served rows. Warns (rather than
/// silently overwriting) if the same symbol resolves to more than one
/// instrument id within that set — e.g. a dual listing across two MICs the
/// same broker serves — and deterministically keeps the first one seen
/// instead of depending on iteration order.
fn build_symbol_to_id(rows: &[(i64, String)]) -> HashMap<String, i64> {
    let mut map: HashMap<String, i64> = HashMap::new();
    for (id, symbol) in rows {
        match map.get(symbol) {
            Some(existing) if *existing != *id => {
                warn!(
                    symbol,
                    existing_id = existing,
                    new_id = id,
                    "daily stats: symbol maps to more than one instrument id within one broker's served venues, keeping the first"
                );
            }
            _ => {
                map.insert(symbol.clone(), *id);
            }
        }
    }
    map
}

/// The union of two instrument-id lists, deduplicated. A pure merge step,
/// factored out so the "did we dedupe/union correctly" question doesn't need
/// a database to answer.
pub fn interesting_instrument_ids(watchlisted: &[i64], held: &[i64]) -> Vec<i64> {
    let mut set: HashSet<i64> = HashSet::new();
    set.extend(watchlisted.iter().copied());
    set.extend(held.iter().copied());
    set.into_iter().collect()
}

/// Parses each row's instrument_id as i64, warning (not just silently
/// dropping, matching every other failure path in this file) about any row
/// that doesn't parse — that should never happen since both source tables
/// only ever get their instrument_id from a real instrument.id, but a
/// silent drop here would be invisible if it ever did.
fn parse_ids_warning_on_failure(rows: Vec<String>, source: &str) -> Vec<i64> {
    rows.into_iter()
        .filter_map(|s| match s.parse::<i64>() {
            Ok(id) => Some(id),
            Err(e) => {
                warn!(error = %e, value = %s, source, "daily stats: dropping instrument_id that doesn't parse as i64");
                None
            }
        })
        .collect()
}

async fn watchlisted_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT DISTINCT instrument_id FROM watchlist_item")
        .fetch_all(pool)
        .await
        .map(|rows| parse_ids_warning_on_failure(rows, "watchlist_item"))
}

async fn held_instrument_ids(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT instrument_id FROM position WHERE net_qty <> 0",
    )
    .fetch_all(pool)
    .await
    .map(|rows| parse_ids_warning_on_failure(rows, "position"))
}

/// instrument.symbol/venue for a set of ids, as `(id, symbol, venue)` — the
/// poller needs symbols to call each adapter with, and venue to know WHICH
/// adapter it's even valid to call with a given symbol (see
/// `broker_serves_venue`). The stores it writes to are keyed by id (matching
/// `MarkStore`).
async fn symbols_for(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String, String)>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id, symbol, venue FROM instrument WHERE id = ANY($1) AND status = 'ACTIVE'",
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

    // Only one environment per broker_code is asked (see
    // select_one_environment_per_broker's doc) — otherwise BINANCE/PAPER and
    // BINANCE/LIVE, if both registered, would both answer for the same
    // symbols and whichever responds second would silently overwrite the
    // other's price.
    let registered: Vec<(String, String)> = registry.iter().map(|(key, _)| key.clone()).collect();
    let selected_environments = select_one_environment_per_broker(&registered);

    for (broker_code, environment) in selected_environments {
        let Some(adapter) = registry.get(&broker_code, &environment) else {
            continue;
        };

        // The subset of the interesting set THIS broker actually serves —
        // never the full cross-venue list. Binance's ticker/24hr rejects its
        // entire request if even one symbol in the batch isn't a Binance
        // symbol (live-verified), so a Binance adapter asked about an equity
        // symbol alongside its crypto ones would silently stop updating
        // anything, every poll.
        let served: Vec<(i64, String)> = by_id
            .iter()
            .filter(|(_, _, venue)| broker_serves_venue(&broker_code, venue))
            .map(|(id, symbol, _)| (*id, symbol.clone()))
            .collect();
        if served.is_empty() {
            continue;
        }

        let symbol_to_id = build_symbol_to_id(&served);
        let symbols: Vec<String> = served.into_iter().map(|(_, sym)| sym).collect();

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
                    info!(broker_code = %broker_code, environment = %environment, updated, "daily stats: refreshed");
                }
            }
            Err(crate::adapters::BrokerError::NotConfigured(_)) => {
                // This adapter doesn't support it. Not an error — most won't.
            }
            Err(e) => {
                warn!(broker_code = %broker_code, environment = %environment, error = %e, "daily stats: adapter call failed, others continue");
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

    // broker_serves_venue is the fix for the live-verified Binance bug: a
    // Binance adapter must never be handed an equity symbol alongside its
    // crypto ones, or the WHOLE request is rejected.
    #[test]
    fn binance_serves_only_binance_venue() {
        assert!(broker_serves_venue("BINANCE", "BINANCE"));
        assert!(!broker_serves_venue("BINANCE", "XNAS"));
        assert!(!broker_serves_venue("BINANCE", "BYBIT"));
    }

    #[test]
    fn alpaca_serves_equity_mics_not_crypto_venues() {
        assert!(broker_serves_venue("ALPACA", "XNAS"));
        assert!(broker_serves_venue("ALPACA", "XNYS"));
        assert!(!broker_serves_venue("ALPACA", "BINANCE"));
        assert!(!broker_serves_venue("ALPACA", "BYBIT"));
    }

    #[test]
    fn unknown_broker_code_serves_nothing() {
        assert!(!broker_serves_venue("IBKR", "XNAS"));
    }

    // select_one_environment_per_broker is the fix for the second symptom: two
    // environments of the same broker must never both answer for that
    // broker's symbols.
    #[test]
    fn prefers_live_over_paper_for_the_same_broker() {
        let registered = vec![
            ("BINANCE".to_string(), "PAPER".to_string()),
            ("BINANCE".to_string(), "LIVE".to_string()),
        ];
        let selected = select_one_environment_per_broker(&registered);
        assert_eq!(selected, vec![("BINANCE".to_string(), "LIVE".to_string())]);
    }

    #[test]
    fn prefers_live_regardless_of_input_order() {
        let registered = vec![
            ("BINANCE".to_string(), "LIVE".to_string()),
            ("BINANCE".to_string(), "PAPER".to_string()),
        ];
        let selected = select_one_environment_per_broker(&registered);
        assert_eq!(selected, vec![("BINANCE".to_string(), "LIVE".to_string())]);
    }

    #[test]
    fn keeps_one_environment_per_distinct_broker() {
        let registered = vec![
            ("ALPACA".to_string(), "PAPER".to_string()),
            ("BINANCE".to_string(), "LIVE".to_string()),
        ];
        let selected = select_one_environment_per_broker(&registered);
        assert_eq!(
            selected,
            vec![
                ("ALPACA".to_string(), "PAPER".to_string()),
                ("BINANCE".to_string(), "LIVE".to_string()),
            ]
        );
    }

    #[test]
    fn single_environment_per_broker_is_unaffected() {
        let registered = vec![("ALPACA".to_string(), "PAPER".to_string())];
        let selected = select_one_environment_per_broker(&registered);
        assert_eq!(selected, vec![("ALPACA".to_string(), "PAPER".to_string())]);
    }

    #[test]
    fn build_symbol_to_id_keeps_first_id_on_collision() {
        let rows = vec![(1i64, "AAPL".to_string()), (2i64, "AAPL".to_string())];
        let map = build_symbol_to_id(&rows);
        assert_eq!(map.get("AAPL"), Some(&1));
    }

    #[test]
    fn build_symbol_to_id_maps_distinct_symbols() {
        let rows = vec![(1i64, "AAPL".to_string()), (2i64, "MSFT".to_string())];
        let map = build_symbol_to_id(&rows);
        assert_eq!(map.get("AAPL"), Some(&1));
        assert_eq!(map.get("MSFT"), Some(&2));
    }
}
