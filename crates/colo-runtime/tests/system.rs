//! Failure-driven local system checks: restart, durable intent, kill latch and
//! a blocking venue call. OMS database/TLS integration is exercised by the
//! isolated script, not by this network-free test.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use colo_link::v2::{
    Allocation, Authority, Bootstrap, Kill, Received, RuntimeCheckpoint, Timings, ToRuntime,
};
use colo_runtime::{
    AccountIdentity, AssetBalance, InstrumentRules, Intent,
    Operation, OrderType, Runtime, RuntimeConfig, RuntimeState, Side, TimeInForce,
    VenueCapabilities, VenueEvent,
};
use gateway::adapter::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};
use gateway::contract::{BrokerOpenOrder, BrokerOrderState};
use gateway::runtime::{ExecutionCursor, ExecutionPage, RuntimeOrder, RuntimeVenue};
use gateway::runtime::{Trade, TradeKey, TradeProvenance};
use tempfile::TempDir;
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity { account_id: Uuid::new_v4(), venue: "BINANCE".into(), environment: "LIVE".into(), external_account_ref: "sub".into() }
}

fn rules() -> InstrumentRules {
    InstrumentRules {
        symbol: "BTCUSDT".into(), venue: "BINANCE".into(), venue_symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(), quote_asset: "USDT".into(), tick_size: 1.0, step_size: 1.0,
        multiplier: 1.0, min_quantity: None, max_quantity: None, min_notional: None, max_notional: None,
        operations: vec![Operation::Place, Operation::Cancel],
        capabilities: VenueCapabilities { cancel_on_disconnect: false, atomic_cancel_replace: false, open_orders_snapshot: true, order_lookup: true, trade_history: true, balance_snapshot: true },
    }
}

fn bootstrap(killed: Option<u64>) -> Bootstrap {
    Bootstrap {
        instruments: vec![rules()],
        limits: BTreeMap::from([("BTCUSDT".into(), risk::Limits::default())]),
        trading_state: risk::TradingState::Active,
        instrument_states: BTreeMap::new(),
        timings: Timings { heartbeat_secs: 1, authority_secs: 5, market_data_max_age_secs: 2 },
        baseline: RuntimeCheckpoint { sequence: 1, positions: BTreeMap::new(), open_client_ids: vec![], execution_cursor: ExecutionCursor::default(), complete: true },
        asset_baseline: BTreeMap::from([("USDT".into(), 1_000.0)]),
        killed_epoch: killed,
        allocation: Allocation { quote_asset: "USDT".into(), max_notional: 1_000.0, max_position_qty: None },
        recovery_universe: vec!["BTCUSDT".into()],
    }
}

struct Fake {
    submitted: AtomicUsize,
    block: AtomicBool,
    gate: Notify,
    started: Notify,
    tx: std::sync::Mutex<Option<mpsc::Sender<VenueEvent>>>,
}

#[async_trait::async_trait]
impl BrokerAdapter for Fake {
    async fn submit_order(&self, _: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        self.started.notify_waiters();
        if self.block.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        self.submitted.fetch_add(1, Ordering::SeqCst);
        Ok(BrokerOrderResponse { external_order_id: "v-1".into() })
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> { Ok(()) }
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> { Ok(vec![]) }
    async fn order_status(&self, _: &str, _: &str, _: &str) -> Result<BrokerOrderState, BrokerError> { Err(BrokerError::NoSuchOrder("fake".into())) }
}

#[async_trait::async_trait]
impl RuntimeVenue for Fake {
    async fn metadata(&self, _: &[String]) -> Result<Vec<InstrumentRules>, BrokerError> { Ok(vec![rules()]) }
    async fn balances(&self) -> Result<Vec<AssetBalance>, BrokerError> { Ok(vec![AssetBalance { asset: "USDT".into(), free: 1_000.0, locked: 0.0 }]) }
    async fn snapshot_orders(&self) -> Result<Vec<RuntimeOrder>, BrokerError> { Ok(vec![]) }
    async fn executions(&self, cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError> { Ok(ExecutionPage { trades: vec![], next: cursor.clone(), complete: true, missing: vec![] }) }
    async fn lookup(&self, _: &str, _: &str) -> Result<RuntimeOrder, BrokerError> { Err(BrokerError::NoSuchOrder("fake".into())) }
    async fn subscribe(&self, tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError> {
        *self.tx.lock().unwrap() = Some(tx);
        Ok(())
    }
}

async fn start(dir: &TempDir, venue: Arc<Fake>, identity: &AccountIdentity, killed: Option<u64>) -> Runtime {
    let runtime = Runtime::start(
        RuntimeConfig {
            identity: identity.clone(), epoch: 1, bootstrap: bootstrap(killed), journal_path: dir.path().join("journal"),
            max_journal_bytes: 1_000_000, event_queue_capacity: 128,
            control_url: None, events_url: None, credential_reference: None,
            link: None, session_nonce: String::new(),
        },
        venue,
    ).await.unwrap();
    runtime.set_price("BTCUSDT", colo_runtime::InstrumentPrices::new(Some(100.0), Some(100.0), Some(100.0), Instant::now())).await;
    runtime
}

fn intent() -> Intent {
    Intent { instrument: "BTCUSDT".into(), side: Side::Buy, quantity: 1.0, order_type: OrderType::Limit, limit_price: Some(100.0), max_quote_spend: None, time_in_force: TimeInForce::Gtc }
}

async fn grant(runtime: &Runtime) {
    runtime.apply_control(Received::new(ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } }, Instant::now())).await.unwrap();
}

#[tokio::test]
async fn crash_after_send_is_followed_by_durable_journal_replay() {
    let dir = TempDir::new().unwrap();
    let identity = identity();
    let venue = Arc::new(Fake { submitted: AtomicUsize::new(0), block: AtomicBool::new(false), gate: Notify::new(), started: Notify::new(), tx: std::sync::Mutex::new(None) });
    let runtime = start(&dir, venue.clone(), &identity, None).await;
    grant(&runtime).await;
    let submission = runtime.submit(intent()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let runtime = start(&dir, venue, &identity, None).await;
    grant(&runtime).await;
    // The journal's intent is replayed, so the next sequence is never reused.
    let next = runtime.submit(intent()).await.unwrap();
    assert!(next.sequence > submission.sequence);
}

#[tokio::test]
async fn a_persisted_kill_survives_restart_and_blocks_admission() {
    let dir = TempDir::new().unwrap();
    let identity = identity();
    let venue = Arc::new(Fake { submitted: AtomicUsize::new(0), block: AtomicBool::new(false), gate: Notify::new(), started: Notify::new(), tx: std::sync::Mutex::new(None) });
    let runtime = start(&dir, venue.clone(), &identity, None).await;
    grant(&runtime).await;
    runtime.apply_control(Received::new(ToRuntime::Kill { kill: Kill { command_id: "kill".into() } }, Instant::now())).await.unwrap();
    runtime.shutdown().await.unwrap();
    drop(runtime);
    let runtime = start(&dir, venue, &identity, Some(1)).await;
    grant(&runtime).await;
    assert!(matches!(runtime.submit(intent()).await, Err(colo_runtime::RuntimeError::KillLatched)));
    assert_eq!(runtime.state().await, RuntimeState::Suspended);
}

#[tokio::test]
async fn a_ten_second_venue_call_cannot_stall_the_watchdog_path() {
    let dir = TempDir::new().unwrap();
    let identity = identity();
    let venue = Arc::new(Fake { submitted: AtomicUsize::new(0), block: AtomicBool::new(true), gate: Notify::new(), started: Notify::new(), tx: std::sync::Mutex::new(None) });
    let runtime = start(&dir, venue.clone(), &identity, None).await;
    grant(&runtime).await;
    runtime.submit(intent()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), venue.started.notified()).await.expect("submission started");
    runtime.apply_control(Received::new(ToRuntime::Kill { kill: Kill { command_id: "kill".into() } }, Instant::now())).await.unwrap();
    assert_eq!(runtime.state().await, RuntimeState::Suspended);
    venue.gate.notify_waiters();
}

#[tokio::test]
async fn a_post_start_fill_moves_position_and_leaves() {
    let dir = TempDir::new().unwrap();
    let identity = identity();
    let venue = Arc::new(Fake { submitted: AtomicUsize::new(0), block: AtomicBool::new(false), gate: Notify::new(), started: Notify::new(), tx: std::sync::Mutex::new(None) });
    let runtime = start(&dir, venue.clone(), &identity, None).await;
    grant(&runtime).await;
    let submission = runtime.submit(intent()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while venue.submitted.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let tx = venue.tx.lock().unwrap().clone().expect("stream subscribed");
    tx.send(VenueEvent::Trade(Trade {
        key: TradeKey {
            venue: identity.venue.clone(),
            account_id: identity.account_id,
            environment: identity.environment.clone(),
            symbol: "BTCUSDT".into(),
            trade_id: "7".into(),
        },
        venue_order_id: "v-1".into(),
        client_order_id: submission.client_order_id.clone(),
        side: Side::Buy,
        quantity: 0.4,
        price: 100.0,
        executed_at: chrono::Utc::now(),
        fee: None,
        provenance: TradeProvenance::Stream,
    })).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.position("BTCUSDT").await < 0.4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("stream fill applied");
    assert_eq!(runtime.leaves(&submission.client_order_id).await, Some(0.6));
}
