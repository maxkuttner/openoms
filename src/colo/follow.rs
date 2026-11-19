//! Follow streams: one per co-located strategy, on its venue account, with the
//! OMS's follow-only key. Each is a reconciler of the account's connection
//! scoped to that one account (`reconciler::Accounts::Only`): its venue events
//! book through ingest like any connection's, and its pulls fill the gaps.
//! The same events also feed the co-located runtime state (open orders,
//! order flow, the kill fight) and the colo trade ledger.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use sqlx::Row;
use tracing::{error, info, warn};

use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::stream_health::StreamKind;

use gateway::adapter::{BrokerAdapter, InstrumentProvider};
use gateway::venue_event::{ExecutionSource, VenueEvent};

use crate::app_state::AppState;
use crate::reconciler::Accounts;

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
        let account = match followed_account(state, &code).await {
            Ok(Some(account)) => account,
            Ok(None) => {
                error!(strategy = %code, "co-located: the strategy has no BINANCE account; not followed");
                continue;
            }
            Err(e) => {
                error!(strategy = %code, error = %e, "co-located: account unreadable; not followed");
                continue;
            }
        };
        state.colo().set_adapter(&code, adapter.clone());
        let health = state.stream_health().handle(&stream_code(&code), environment, StreamKind::Execution);
        let handle = tokio::spawn(follow(state.clone(), code.clone(), environment, adapter, account, health));
        state.streams().abort_and_remove(&stream_code(&code)).await;
        state.streams().insert(stream_code(&code), handle);
        running.insert(code.clone(), fingerprint(env, sealed));
        info!(strategy = %code, environment, "co-located: following the strategy's account");
    }
}

/// The account a strategy's follow covers: its BINANCE account, with the
/// connection it belongs to.
#[derive(Debug, Clone)]
struct FollowedAccount {
    id: uuid::Uuid,
    external_ref: String,
    connection: String,
}

async fn followed_account(state: &AppState, code: &str) -> Result<Option<FollowedAccount>, sqlx::Error> {
    let row: Option<(uuid::Uuid, Option<String>, String)> = sqlx::query_as(
        "SELECT a.id, a.external_account_ref, a.broker_connection_code FROM strategy_connection sc \
         JOIN account a ON a.id = sc.account_id \
         WHERE sc.strategy_code = $1 AND sc.broker_code = 'BINANCE'",
    )
    .bind(code)
    .fetch_optional(state.pool())
    .await?;
    Ok(row.and_then(|(id, external_ref, connection)| {
        Some(FollowedAccount { id, external_ref: external_ref.filter(|r| !r.is_empty())?, connection })
    }))
}

/// One follow, for as long as the stream registry keeps it: the account's
/// user-data stream, teed to the co-located runtime and to a reconciler of
/// the account's connection scoped to this account. Every event carries the
/// account's ref, so ingest books it on the sub-account, never the
/// connection's main account.
async fn follow(
    state: AppState,
    code: String,
    environment: &'static str,
    adapter: Arc<BinanceAdapter>,
    account: FollowedAccount,
    health: gateway::stream_health::StreamHandle,
) {
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(gateway::contract::ENGINE_CHANNEL_CAPACITY);
    let (recon_tx, recon_rx) = tokio::sync::mpsc::channel(gateway::contract::ENGINE_CHANNEL_CAPACITY);
    let reconciler = crate::reconciler::spawn_with(
        account.connection.clone(),
        adapter.clone() as Arc<dyn ExecutionSource>,
        recon_rx,
        state.engine().clone(),
        state.pool().clone(),
        Some(adapter.clone() as Arc<dyn BrokerAdapter>),
        Some(adapter.clone() as Arc<dyn InstrumentProvider>),
        Accounts::Only(account.id),
    );
    let stream = gateway::connectors::binance::stream::run(environment, adapter, health, stream_tx);
    let hub = state.colo().clone();
    let tee = async move {
        let _reconciler = reconciler;
        while let Some(mut event) = stream_rx.recv().await {
            set_account_ref(&mut event, &account.external_ref);
            // Booking first; the runtime view must never hold it up.
            let observed = event.clone();
            if recon_tx.send(event).await.is_err() {
                break;
            }
            hub.observe(&code, observed);
        }
    };
    tokio::join!(stream, tee);
}

/// Stamp a follow event with the followed account's ref.
fn set_account_ref(event: &mut VenueEvent, account_ref: &str) {
    match event {
        VenueEvent::Fill(f) => f.account_ref = account_ref.to_string(),
        VenueEvent::Order(o) => o.account_ref = account_ref.to_string(),
        VenueEvent::Fee { account_ref: r, .. } => *r = account_ref.to_string(),
        VenueEvent::Connected | VenueEvent::Disconnected => {}
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
    fn every_follow_event_carries_the_followed_account() {
        use gateway::venue_event::{VenueFill, VenueOrderStatus, VenueOrderUpdate};
        let mut fill = VenueEvent::Fill(VenueFill {
            exec_id: "1".into(), venue_order_id: "9".into(), client_order_id: None, account_ref: String::new(),
            symbol: "BTCUSDT".into(), buy: true, qty: 1.0, price: 1.0, fee: None, executed_at: None,
        });
        let mut order = VenueEvent::Order(VenueOrderUpdate {
            venue_order_id: "9".into(), client_order_id: None, account_ref: String::new(), symbol: "BTCUSDT".into(),
            buy: true, order_type: "LIMIT".into(), qty: 1.0, limit_px: None, status: VenueOrderStatus::Working, cum_qty: 0.0, ts: None, created_at: None,
        });
        super::set_account_ref(&mut fill, "sub-1");
        super::set_account_ref(&mut order, "sub-1");
        assert!(matches!(fill, VenueEvent::Fill(f) if f.account_ref == "sub-1"));
        assert!(matches!(order, VenueEvent::Order(o) if o.account_ref == "sub-1"));
    }
}
