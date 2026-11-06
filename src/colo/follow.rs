//! Follow streams: one per co-located strategy, on its venue account, with the
//! OMS's follow-only key; and filling a stream's gap from the trade history.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use sqlx::Row;
use tracing::{error, info, warn};

use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::stream_health::StreamKind;

use super::{booking, ColoHub};
use crate::app_state::AppState;
use crate::strategies::Strategy;

#[derive(Deserialize)]
struct FollowKey {
    api_key: String,
    private_key: String,
}

/// The `StreamRegistry` key of a strategy's follow stream.
pub fn stream_code(strategy_code: &str) -> String {
    format!("colo-{strategy_code}")
}

/// Which follow streams to (re)start and which to stop, from what runs and
/// what the registry wants (strategy code → a fingerprint of its follow key
/// and environment).
#[derive(Debug, PartialEq)]
pub struct FollowPlan {
    pub start: Vec<String>,
    pub stop: Vec<String>,
}

pub fn follow_plan(running: &BTreeMap<String, u64>, wanted: &BTreeMap<String, u64>) -> FollowPlan {
    FollowPlan {
        start: wanted.iter().filter(|(code, f)| running.get(*code) != Some(f)).map(|(code, _)| code.clone()).collect(),
        stop: running.keys().filter(|code| !wanted.contains_key(*code)).cloned().collect(),
    }
}

fn fingerprint(environment: &str, sealed: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (environment, sealed).hash(&mut h);
    h.finish()
}

/// Bring the follow streams in line with the registry: start one for every
/// co-located strategy with a follow key, restart it when its key or
/// environment changed, and stop the rest. At boot and after every registry
/// write.
pub async fn sync_follows(state: &AppState) {
    let mut running = state.colo().followed.lock().await;
    let rows = match sqlx::query(
        "SELECT code, follow_environment, follow_credentials FROM strategy \
         WHERE mode = 'COLOCATED' AND follow_credentials IS NOT NULL AND follow_environment IS NOT NULL",
    )
    .fetch_all(state.pool())
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!(error = %e, "co-located: could not load followed accounts");
            return;
        }
    };
    let wanted: BTreeMap<String, (String, Vec<u8>)> = rows
        .into_iter()
        .map(|row| (row.get("code"), (row.get("follow_environment"), row.get("follow_credentials"))))
        .collect();
    let plan = follow_plan(&running, &wanted.iter().map(|(code, (env, sealed))| (code.clone(), fingerprint(env, sealed))).collect());
    for code in &plan.stop {
        state.streams().abort_and_remove(&stream_code(code)).await;
        state.colo().adapters.lock().unwrap().remove(code);
        running.remove(code);
        info!(strategy = %code, "co-located: stopped following the strategy's account");
    }
    if plan.start.is_empty() {
        return;
    }
    let Ok(Some(key)) = crate::admin::resolve_master_key() else {
        warn!("co-located: no master key; no account can be followed");
        return;
    };
    for code in plan.start {
        let (env, sealed) = &wanted[&code];
        let environment: &'static str = if env == "LIVE" { "LIVE" } else { "PAPER" };
        let adapter = crate::secrets::open(&key, &crate::strategies::admin::follow_aad(&code), sealed)
            .map_err(|e| format!("{e:?}"))
            .and_then(|plain| serde_json::from_slice::<FollowKey>(&plain).map_err(|e| e.to_string()))
            .and_then(|k| BinanceAdapter::new(k.api_key, &k.private_key, environment));
        let adapter = match adapter {
            Ok(a) => Arc::new(a),
            Err(e) => {
                error!(strategy = %code, error = %e, "co-located: follow key unusable");
                continue;
            }
        };
        state.colo().set_adapter(&code, adapter.clone());
        let health = state.stream_health().handle(&stream_code(&code), environment, StreamKind::Execution);
        let (sink, follow_code) = (state.colo().follow_handle(), code.clone());
        let handle = tokio::spawn(gateway::connectors::binance::stream::follow(environment, follow_code, adapter, health, sink));
        state.streams().abort_and_remove(&stream_code(&code)).await;
        state.streams().insert(stream_code(&code), handle);
        running.insert(code.clone(), fingerprint(env, sealed));
        info!(strategy = %code, environment, "co-located: following the strategy's account");
    }
}

/// How far back gap recon looks: Binance's trade history answers 24 hours
/// per start time.
pub const DAY_MS: i64 = 24 * 3_600_000;

/// Where gap recon starts: a day back, but never before following began.
pub fn recon_start(follow_since_ms: Option<i64>, now_ms: i64) -> i64 {
    follow_since_ms.map_or(now_ms - DAY_MS, |since| since.max(now_ms - DAY_MS))
}

/// The next page's first trade id after a full page.
pub fn next_page(page_len: usize, last_trade_id: Option<i64>) -> Option<i64> {
    (page_len >= 1000).then_some(last_trade_id?).map(|id| id + 1)
}

/// Every pair gap recon asks about: booked, open now, or seen on the account.
pub fn recon_symbols(
    booked: impl IntoIterator<Item = String>,
    open: impl IntoIterator<Item = String>,
    seen: impl IntoIterator<Item = String>,
) -> std::collections::BTreeSet<String> {
    booked.into_iter().chain(open).chain(seen).collect()
}

/// After a (re)connect: book every trade of the last day (since following
/// began) on every pair the account has touched. Booking skips what is
/// already booked, so a fill that failed earlier is retried here too.
pub async fn fill_gap(state: &AppState, hub: &ColoHub, strategy: &Strategy, adapter: &BinanceAdapter) {
    use gateway::adapter::BrokerAdapter;
    let open = adapter.open_orders().await.unwrap_or_default();
    let seen = {
        let mut runtimes = hub.runtimes.lock().unwrap();
        let runtime = runtimes.entry(strategy.code.clone()).or_default();
        runtime.open.retain(|venue_id, _| open.iter().any(|o| &o.venue_order_id == venue_id));
        runtime.seen_symbols.iter().cloned().collect::<Vec<_>>()
    };
    let booked = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT bi.broker_symbol FROM order_state os \
         JOIN broker_instrument bi ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = 'BINANCE' \
         WHERE os.account_id = $1 AND os.origin = 'EXTERNAL'",
    )
    .bind(strategy.connections.get("BINANCE").copied().unwrap_or_default())
    .fetch_all(state.pool())
    .await
    .unwrap_or_default();
    let since: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("SELECT follow_since FROM strategy WHERE code = $1")
        .bind(&strategy.code)
        .fetch_optional(state.pool())
        .await
        .ok()
        .flatten()
        .flatten();
    let start = recon_start(since.map(|t| t.timestamp_millis()), chrono::Utc::now().timestamp_millis());
    let symbols = recon_symbols(booked, open.into_iter().map(|o| o.symbol), seen);
    let mut booked_now = 0usize;
    for symbol in symbols {
        let mut from: Option<i64> = None;
        loop {
            let page = match adapter.my_trades(&symbol, from, Some(start)).await {
                Ok(page) => page,
                Err(e) => {
                    warn!(strategy = %strategy.code, %symbol, error = %e, "co-located: trade history unavailable; gap not filled");
                    break;
                }
            };
            let last = page.last().and_then(|f| f.trade_id.parse::<i64>().ok());
            for fill in &page {
                match booking::book_fill(state, strategy, adapter, fill).await {
                    Ok(booking::Booked::New) => booked_now += 1,
                    Ok(booking::Booked::AlreadyBooked) => {}
                    Err(e) => error!(strategy = %strategy.code, trade = %fill.trade_id, error = %e, "co-located: gap fill not booked; retried at the next reconnect"),
                }
            }
            match next_page(page.len(), last) {
                Some(next) => from = Some(next),
                None => break,
            }
        }
    }
    if booked_now > 0 {
        info!(strategy = %strategy.code, booked_now, "co-located: gap filled from the trade history");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn follows_start_stop_and_restart_with_the_registry() {
        let running = BTreeMap::from([("keep".to_string(), 1u64), ("rekeyed".to_string(), 2), ("gone".to_string(), 3)]);
        let wanted = BTreeMap::from([("keep".to_string(), 1u64), ("rekeyed".to_string(), 9), ("new".to_string(), 4)]);
        let plan = follow_plan(&running, &wanted);
        assert_eq!(plan.start, ["new", "rekeyed"]);
        assert_eq!(plan.stop, ["gone"]);
    }

    use super::*;

    #[test]
    fn recon_looks_back_a_day_but_never_before_following_began() {
        let now = 10 * DAY_MS;
        assert_eq!(recon_start(None, now), now - DAY_MS);
        assert_eq!(recon_start(Some(now - 3_600_000), now), now - 3_600_000);
        assert_eq!(recon_start(Some(now - 5 * DAY_MS), now), now - DAY_MS);
    }

    #[test]
    fn the_trade_history_is_paged_until_a_short_page() {
        assert_eq!(next_page(1000, Some(41_999)), Some(42_000));
        assert_eq!(next_page(999, Some(41_999)), None);
        assert_eq!(next_page(1000, None), None);
    }

    #[test]
    fn recon_covers_every_pair_seen_not_only_booked_ones() {
        let symbols = recon_symbols(["BTCUSDT".to_string()], ["ETHBTC".to_string()], ["SOLUSDT".to_string(), "BTCUSDT".to_string()]);
        assert_eq!(symbols.into_iter().collect::<Vec<_>>(), ["BTCUSDT", "ETHBTC", "SOLUSDT"]);
    }
}
