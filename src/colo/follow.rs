//! Follow streams: one per co-located strategy, on its venue account, with the
//! OMS's follow-only key; and filling a stream's gap from the trade history.

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

/// Start (or restart) the follow stream of every co-located strategy that has
/// a follow key.
pub async fn start_follows(state: &AppState) {
    let Ok(Some(key)) = crate::admin::resolve_master_key() else {
        warn!("co-located: no master key; no account can be followed");
        return;
    };
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
    for row in rows {
        let code: String = row.get("code");
        let environment: &'static str = if row.get::<String, _>("follow_environment") == "LIVE" { "LIVE" } else { "PAPER" };
        let sealed: Vec<u8> = row.get("follow_credentials");
        let adapter = crate::secrets::open(&key, &crate::strategies::admin::follow_aad(&code), &sealed)
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
        info!(strategy = %code, environment, "co-located: following the strategy's account");
    }
}

/// After a (re)connect: book every trade after the last one booked, per pair
/// the strategy has traded, and refresh the open-order view.
pub async fn fill_gap(state: &AppState, hub: &ColoHub, strategy: &Strategy, adapter: &BinanceAdapter) {
    use gateway::adapter::BrokerAdapter;
    if let Ok(open) = adapter.open_orders().await {
        let mut runtimes = hub.runtimes.lock().unwrap();
        let runtime = runtimes.entry(strategy.code.clone()).or_default();
        runtime.open.retain(|venue_id, _| open.iter().any(|o| &o.venue_order_id == venue_id));
    }
    let last = sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT bi.broker_symbol, MAX(os.venue_trade_id) FROM order_state os \
         JOIN broker_instrument bi ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = 'BINANCE' \
         WHERE os.portfolio_id = $1 AND os.origin = 'EXTERNAL' GROUP BY bi.broker_symbol",
    )
    .bind(strategy.portfolio_id)
    .fetch_all(state.pool())
    .await;
    let last = match last {
        Ok(rows) => rows,
        Err(e) => {
            error!(strategy = %strategy.code, error = %e, "co-located: gap recon could not read booked trades");
            return;
        }
    };
    for (symbol, last_trade) in last {
        match adapter.my_trades(&symbol, last_trade.map(|t| t + 1)).await {
            Ok(fills) => {
                for fill in fills {
                    if let Err(e) = booking::book_fill(state, strategy, adapter, &fill).await {
                        error!(strategy = %strategy.code, trade = %fill.trade_id, error = %e, "co-located: gap fill not booked");
                    }
                }
            }
            Err(e) => warn!(strategy = %strategy.code, %symbol, error = %e, "co-located: trade history unavailable; gap not filled"),
        }
    }
}
