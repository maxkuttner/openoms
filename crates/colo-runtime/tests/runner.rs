use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use colo_link::v2::{
    Allocation, Authority, Bootstrap, FromRuntime, FromRuntimeEnvelope, Kill, ProtocolError,
    Received, RuntimeCheckpoint, RuntimeLink, Timings, ToRuntime,
};
use colo_runtime::{
    AccountIdentity, AssetBalance, InstrumentRules, Intent,
    Operation, OrderType, Runtime, RuntimeConfig, RuntimeState, Side, TimeInForce,
    VenueCapabilities, VenueEvent,
};
use gateway::adapter::{
    BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse,
};
use gateway::contract::{BrokerOpenOrder, BrokerOrderState};
use gateway::runtime::{ExecutionCursor, ExecutionPage, RuntimeVenue};
use tempfile::TempDir;
use tokio::sync::{mpsc, Notify};
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "sub-1".into(),
    }
}

fn rules() -> InstrumentRules {
    InstrumentRules {
        symbol: "BTCUSDT".into(),
        venue: "BINANCE".into(),
        venue_symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        tick_size: 1.0,
        step_size: 1.0,
        multiplier: 1.0,
        min_quantity: None,
        max_quantity: None,
        min_notional: None,
        max_notional: None,
        operations: vec![Operation::Place, Operation::Cancel],
        capabilities: VenueCapabilities {
            cancel_on_disconnect: false,
            atomic_cancel_replace: false,
            open_orders_snapshot: true,
            order_lookup: true,
            trade_history: true,
            balance_snapshot: true,
        },
    }
}

fn bootstrap() -> Bootstrap {
    Bootstrap {
        instruments: vec![rules()],
        limits: BTreeMap::from([("BTCUSDT".into(), risk::Limits::default())]),
        trading_state: risk::TradingState::Active,
        instrument_states: BTreeMap::new(),
        timings: Timings { heartbeat_secs: 1, authority_secs: 5, market_data_max_age_secs: 2 },
        baseline: RuntimeCheckpoint {
            sequence: 1,
            positions: BTreeMap::new(),
            open_client_ids: vec![],
            execution_cursor: ExecutionCursor::default(),
            complete: true,
        },
        asset_baseline: BTreeMap::from([("USDT".into(), 1_000.0)]),
        killed_epoch: None,
        allocation: Allocation {
            quote_asset: "USDT".into(),
            max_notional: 1_000.0,
            max_position_qty: None,
        },
        recovery_universe: vec!["BTCUSDT".into()],
    }
}

struct FakeVenue {
    submitted: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    /// Cancel attempts that fail before one succeeds; `usize::MAX` fails all.
    cancel_failures: AtomicUsize,
    /// The next submit fails with a network error: the venue may have the order.
    submit_unclear: AtomicBool,
    /// Lookups find the order resting; otherwise they fail with a network error.
    lookup_finds: AtomicBool,
    lookups: AtomicUsize,
    block: AtomicBool,
    gate: Arc<Notify>,
    started: Arc<Notify>,
}

impl FakeVenue {
    fn new() -> Self {
        Self {
            submitted: Arc::new(AtomicUsize::new(0)),
            cancels: Arc::new(AtomicUsize::new(0)),
            cancel_failures: AtomicUsize::new(0),
            submit_unclear: AtomicBool::new(false),
            lookup_finds: AtomicBool::new(false),
            lookups: AtomicUsize::new(0),
            block: AtomicBool::new(false),
            gate: Arc::new(Notify::new()),
            started: Arc::new(Notify::new()),
        }
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for FakeVenue {
    async fn submit_order(&self, _: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        self.started.notify_waiters();
        if self.block.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        self.submitted.fetch_add(1, Ordering::SeqCst);
        if self.submit_unclear.swap(false, Ordering::SeqCst) {
            return Err(BrokerError::Network("timed out".into()));
        }
        Ok(BrokerOrderResponse { external_order_id: "v-1".into() })
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        let left = self.cancel_failures.load(Ordering::SeqCst);
        if left > 0 {
            if left != usize::MAX {
                self.cancel_failures.store(left - 1, Ordering::SeqCst);
            }
            return Err(BrokerError::Network("venue busy".into()));
        }
        Ok(())
    }
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> { Ok(vec![]) }
    async fn order_status(&self, _: &str, _: &str, _: &str) -> Result<BrokerOrderState, BrokerError> {
        Err(BrokerError::NoSuchOrder("fake".into()))
    }
}

#[async_trait::async_trait]
impl RuntimeVenue for FakeVenue {
    async fn metadata(&self, _: &[String]) -> Result<Vec<InstrumentRules>, BrokerError> { Ok(vec![rules()]) }
    async fn balances(&self) -> Result<Vec<AssetBalance>, BrokerError> {
        Ok(vec![AssetBalance { asset: "USDT".into(), free: 1_000.0, locked: 0.0 }])
    }
    async fn snapshot_orders(&self) -> Result<Vec<colo_runtime::RuntimeOrder>, BrokerError> { Ok(vec![]) }
    async fn executions(&self, cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError> {
        Ok(ExecutionPage { trades: vec![], next: cursor.clone(), complete: true, missing: vec![] })
    }
    async fn lookup(&self, client_order_id: &str, symbol: &str) -> Result<colo_runtime::RuntimeOrder, BrokerError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if !self.lookup_finds.load(Ordering::SeqCst) {
            return Err(BrokerError::Network("lookup timed out".into()));
        }
        Ok(colo_runtime::RuntimeOrder {
            client_order_id: client_order_id.into(),
            venue_order_id: "v-1".into(),
            symbol: symbol.into(),
            side: Side::Buy,
            original_quantity: 1.0,
            executed_quantity: 0.0,
            leaves_quantity: 1.0,
            limit_price: Some(100.0),
            state: colo_runtime::RuntimeOrderState::New,
            original_time: chrono::Utc::now(),
        })
    }
    async fn subscribe(&self, _: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError> { Ok(()) }
}

fn intent(qty: f64) -> Intent {
    Intent {
        instrument: "BTCUSDT".into(),
        side: Side::Buy,
        quantity: qty,
        order_type: OrderType::Limit,
        limit_price: Some(100.0),
        max_quote_spend: None,
        time_in_force: TimeInForce::Gtc,
    }
}

async fn start() -> (Runtime, Arc<FakeVenue>, TempDir) {
    let dir = TempDir::new().unwrap();
    let venue = Arc::new(FakeVenue::new());
    let runtime = Runtime::start(
        RuntimeConfig {
            identity: identity(),
            epoch: 1,
            bootstrap: bootstrap(),
            journal_path: dir.path().join("journal"),
            max_journal_bytes: 1_000_000,
            event_queue_capacity: 128,
            control_url: None,
            events_url: None,
            credential_reference: None,
            link: None,
            session_nonce: String::new(),
        },
        venue.clone(),
    )
    .await
    .unwrap();
    runtime
        .set_price(
            "BTCUSDT",
            colo_runtime::InstrumentPrices::new(Some(100.0), Some(100.0), Some(100.0), Instant::now()),
        )
        .await;
    (runtime, venue, dir)
}

struct FakeLink {
    control: tokio::sync::Mutex<mpsc::Receiver<Received<ToRuntime>>>,
    events: std::sync::Mutex<Vec<FromRuntimeEnvelope>>,
}

#[async_trait::async_trait]
impl RuntimeLink for FakeLink {
    async fn recv_control(&self) -> Option<Received<ToRuntime>> {
        self.control.lock().await.recv().await
    }
    async fn send_events(&self, envelope: FromRuntimeEnvelope) -> Result<(), ProtocolError> {
        self.events.lock().unwrap().push(envelope);
        Ok(())
    }
}

#[tokio::test]
async fn an_injected_link_drives_control_and_receives_reported_events() {
    let dir = TempDir::new().unwrap();
    let venue = Arc::new(FakeVenue::new());
    let (control_tx, control_rx) = mpsc::channel(8);
    let link = Arc::new(FakeLink {
        control: tokio::sync::Mutex::new(control_rx),
        events: std::sync::Mutex::new(Vec::new()),
    });
    let runtime = Runtime::start(
        RuntimeConfig {
            identity: identity(),
            epoch: 1,
            bootstrap: bootstrap(),
            journal_path: dir.path().join("journal"),
            max_journal_bytes: 1_000_000,
            event_queue_capacity: 128,
            control_url: None,
            events_url: None,
            credential_reference: None,
            link: Some(link.clone()),
            session_nonce: "n1".into(),
        },
        venue.clone(),
    )
    .await
    .unwrap();
    runtime
        .set_price("BTCUSDT", colo_runtime::InstrumentPrices::new(Some(100.0), Some(100.0), Some(100.0), Instant::now()))
        .await;
    control_tx
        .send(Received::new(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
            Instant::now(),
        ))
        .await
        .unwrap();
    for _ in 0..100 {
        if runtime.state().await == RuntimeState::Ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut submitted = false;
    for _ in 0..100 {
        if runtime.submit(intent(1.0)).await.is_ok() {
            submitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(submitted, "authority was not applied");
    for _ in 0..100 {
        if link.events.lock().unwrap().iter().any(|e| matches!(e.message, FromRuntime::Events { .. })) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(link
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e.message, FromRuntime::Events { .. })));
    runtime.shutdown().await.unwrap();
}

async fn grant(runtime: &Runtime) {
    runtime
        .apply_control(Received::new(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
            Instant::now(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn no_submit_before_ready_and_authority() {
    let (runtime, venue, _dir) = start().await;
    assert!(runtime.submit(intent(1.0)).await.is_err());
    assert_eq!(venue.submitted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_durable_intent_is_sent_after_authority() {
    let (runtime, venue, _dir) = start().await;
    grant(&runtime).await;
    let submission = runtime.submit(intent(1.0)).await.unwrap();
    assert!(submission.sequence >= 1);
    for _ in 0..100 {
        if venue.submitted.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(venue.submitted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn journal_failure_stops_the_send() {
    let (runtime, venue, _dir) = start().await;
    grant(&runtime).await;
    runtime.fail_next_journal_write_for_test();
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::JournalIo(_))
    ));
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::NotReady(_))
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(venue.submitted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_blocking_venue_call_does_not_block_kill() {
    let (runtime, venue, _dir) = start().await;
    venue.block.store(true, Ordering::SeqCst);
    grant(&runtime).await;
    runtime.submit(intent(1.0)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), venue.started.notified())
        .await
        .expect("venue call started");
    runtime
        .apply_control(Received::new(
            ToRuntime::Kill { kill: Kill { command_id: "k".into() } },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert_eq!(runtime.state().await, RuntimeState::Suspended);
    venue.gate.notify_waiters();
}

#[tokio::test]
async fn a_halted_policy_reaches_the_book() {
    let (runtime, venue, _dir) = start().await;
    grant(&runtime).await;
    runtime
        .apply_control(Received::new(
            ToRuntime::Policy {
                policy: colo_link::v2::Policy {
                    revision: 2,
                    limits: BTreeMap::new(),
                    trading_state: risk::TradingState::Halted,
                    instrument_states: BTreeMap::new(),
                },
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::Rejected(r)) if r.code == "trading_halted"
    ));
    assert_eq!(venue.submitted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_killed_runtime_cannot_submit() {
    let (runtime, _venue, _dir) = start().await;
    grant(&runtime).await;
    runtime
        .apply_control(Received::new(
            ToRuntime::Kill { kill: Kill { command_id: "k".into() } },
            Instant::now(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::KillLatched)
    ));
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Submits one order and waits until the venue has accepted it and the book
/// holds it as live.
async fn live_order(runtime: &Runtime, venue: &FakeVenue) -> String {
    grant(runtime).await;
    let submission = runtime.submit(intent(1.0)).await.unwrap();
    wait_for("venue submit", || venue.submitted.load(Ordering::SeqCst) == 1).await;
    for _ in 0..200 {
        if runtime.next_event().is_some_and(|e| matches!(e, colo_runtime::RuntimeEvent::SubmitOutcome { .. })) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    submission.client_order_id
}

async fn kill(runtime: &Runtime) -> Result<(), colo_runtime::RuntimeError> {
    runtime
        .apply_control(Received::new(
            ToRuntime::Kill { kill: Kill { command_id: "k".into() } },
            Instant::now(),
        ))
        .await
        .map(|_| ())
}

#[tokio::test]
async fn kill_cancels_every_working_order() {
    let (runtime, venue, _dir) = start().await;
    live_order(&runtime, &venue).await;
    kill(&runtime).await.unwrap();
    wait_for("cancel sent", || venue.cancels.load(Ordering::SeqCst) == 1).await;
}

#[tokio::test]
async fn kill_cancels_even_when_the_journal_write_fails() {
    let (runtime, venue, _dir) = start().await;
    live_order(&runtime, &venue).await;
    runtime.fail_next_journal_write_for_test();
    assert!(kill(&runtime).await.is_err(), "the failed kill record is still reported");
    wait_for("cancel sent", || venue.cancels.load(Ordering::SeqCst) == 1).await;
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::KillLatched | colo_runtime::RuntimeError::NotReady(_))
    ));
}

#[tokio::test]
async fn a_failed_cancel_is_retried() {
    let (runtime, venue, _dir) = start().await;
    let id = live_order(&runtime, &venue).await;
    venue.cancel_failures.store(2, Ordering::SeqCst);
    runtime.cancel(&id).await.unwrap();
    wait_for("third cancel attempt", || venue.cancels.load(Ordering::SeqCst) == 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(venue.cancels.load(Ordering::SeqCst), 3, "no attempts after one succeeded");
}

#[tokio::test]
async fn a_cancel_that_keeps_failing_stops_admission() {
    let (runtime, venue, _dir) = start().await;
    let id = live_order(&runtime, &venue).await;
    venue.cancel_failures.store(usize::MAX, Ordering::SeqCst);
    runtime.cancel(&id).await.unwrap();
    // Long enough for every retry to run out.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::NotReady(_))
    ));
    let attempts = venue.cancels.load(Ordering::SeqCst);
    assert!((2..=8).contains(&attempts), "bounded retries, got {attempts}");
}

#[tokio::test]
async fn a_cancel_requested_before_acceptance_is_sent_once_accepted() {
    let (runtime, venue, _dir) = start().await;
    venue.block.store(true, Ordering::SeqCst);
    grant(&runtime).await;
    let submission = runtime.submit(intent(1.0)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), venue.started.notified())
        .await
        .expect("venue call started");
    runtime.cancel(&submission.client_order_id).await.unwrap();
    assert_eq!(venue.cancels.load(Ordering::SeqCst), 0, "no venue id yet");
    venue.gate.notify_waiters();
    wait_for("cancel after acceptance", || venue.cancels.load(Ordering::SeqCst) == 1).await;
}

async fn unclear_submit(runtime: &Runtime, venue: &FakeVenue) {
    grant(runtime).await;
    venue.submit_unclear.store(true, Ordering::SeqCst);
    runtime.submit(intent(1.0)).await.unwrap();
    wait_for("venue submit", || venue.submitted.load(Ordering::SeqCst) == 1).await;
    wait_for("lookup", || venue.lookups.load(Ordering::SeqCst) >= 1).await;
}

#[tokio::test]
async fn an_unclear_submit_stops_admission_until_a_lookup_settles_it() {
    let (runtime, venue, _dir) = start().await;
    unclear_submit(&runtime, &venue).await;
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::NotReady(_))
    ));
    venue.lookup_finds.store(true, Ordering::SeqCst);
    let mut reopened = false;
    for _ in 0..400 {
        if runtime.submit(intent(1.0)).await.is_ok() {
            reopened = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(reopened, "admission stayed stopped after the order was found");
}

#[tokio::test]
async fn an_unsettled_unclear_submit_keeps_admission_stopped() {
    let (runtime, venue, _dir) = start().await;
    unclear_submit(&runtime, &venue).await;
    wait_for("retried lookups", || venue.lookups.load(Ordering::SeqCst) >= 3).await;
    assert!(matches!(
        runtime.submit(intent(1.0)).await,
        Err(colo_runtime::RuntimeError::NotReady(_))
    ));
}

#[tokio::test]
async fn a_found_unclear_order_keeps_its_exposure() {
    let (runtime, venue, _dir) = start().await;
    venue.lookup_finds.store(true, Ordering::SeqCst);
    grant(&runtime).await;
    venue.submit_unclear.store(true, Ordering::SeqCst);
    let submission = runtime.submit(intent(1.0)).await.unwrap();
    wait_for("lookup", || venue.lookups.load(Ordering::SeqCst) >= 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(runtime.leaves(&submission.client_order_id).await, Some(1.0));
}
