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
        // Fixed, so a restarted runtime opens the same journal.
        account_id: Uuid::from_u128(0x6f6d_7331),
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
            fee_allowance_bps: 0.0,
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
    /// The runtime's venue stream, captured at subscribe.
    stream: std::sync::Mutex<Option<mpsc::Sender<VenueEvent>>>,
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
            stream: std::sync::Mutex::new(None),
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
    async fn subscribe(&self, tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError> {
        *self.stream.lock().unwrap() = Some(tx);
        Ok(())
    }
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
    let (runtime, venue) = start_in(&dir).await;
    (runtime, venue, dir)
}

/// Starts a runtime on the journal in `dir`, as a restart would.
async fn start_in(dir: &TempDir) -> (Runtime, Arc<FakeVenue>) {
    start_with(dir, bootstrap()).await
}

async fn start_with(dir: &TempDir, bootstrap: Bootstrap) -> (Runtime, Arc<FakeVenue>) {
    let venue = Arc::new(FakeVenue::new());
    let runtime = Runtime::start(
        RuntimeConfig {
            identity: identity(),
            epoch: 1,
            bootstrap,
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
    (runtime, venue)
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

#[tokio::test]
async fn an_order_cancelled_before_send_is_released_and_survives_restart() {
    let dir = TempDir::new().unwrap();
    let (runtime, venue) = start_in(&dir).await;
    grant(&runtime).await;
    let submission = runtime.submit(intent(1.0)).await.unwrap();
    // Single-threaded test runtime: the send task has not run yet.
    runtime.cancel(&submission.client_order_id).await.unwrap();
    let mut outcome = None;
    for _ in 0..200 {
        if let Some(colo_runtime::RuntimeEvent::SubmitOutcome { outcome: o, .. }) = runtime.next_event() {
            outcome = Some(o);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        matches!(outcome, Some(gateway::runtime::SubmitOutcome::Rejected { .. })),
        "not sent, so not unknown: {outcome:?}"
    );
    assert_eq!(venue.submitted.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.leaves(&submission.client_order_id).await, None, "reservation released");
    runtime.submit(intent(1.0)).await.expect("admission stays open");
    // Let that send finish so its task releases the journal before restart.
    wait_for("second order sent", || venue.submitted.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let (restarted, _venue) = start_in(&dir).await;
    assert_eq!(restarted.state().await, RuntimeState::Ready, "recovery is not stuck on the unsent order");
    assert_eq!(restarted.leaves(&submission.client_order_id).await, None);
}

/// A link whose OMS acknowledges every event batch as soon as it arrives.
struct AckingLink {
    control: tokio::sync::Mutex<mpsc::Receiver<Received<ToRuntime>>>,
    acks: mpsc::Sender<Received<ToRuntime>>,
}

#[async_trait::async_trait]
impl RuntimeLink for AckingLink {
    async fn recv_control(&self) -> Option<Received<ToRuntime>> {
        self.control.lock().await.recv().await
    }
    async fn send_events(&self, envelope: FromRuntimeEnvelope) -> Result<(), ProtocolError> {
        if let FromRuntime::Events { batch } = &envelope.message {
            let ack = colo_link::v2::EventAck { through_sequence: batch.last_sequence() };
            let _ = self.acks.send(Received::new(ToRuntime::EventAck { ack }, Instant::now())).await;
        }
        Ok(())
    }
}

fn fill(client_order_id: &str, trade_id: &str, quantity: f64) -> VenueEvent {
    let id = identity();
    VenueEvent::Trade(colo_runtime::Trade {
        key: colo_runtime::TradeKey {
            venue: id.venue,
            account_id: id.account_id,
            environment: id.environment,
            symbol: "BTCUSDT".into(),
            trade_id: trade_id.into(),
        },
        venue_order_id: "v-1".into(),
        client_order_id: client_order_id.into(),
        side: Side::Buy,
        quantity,
        price: 100.0,
        executed_at: chrono::Utc::now(),
        fee: None,
        provenance: colo_runtime::TradeProvenance::Stream,
    })
}

#[tokio::test]
async fn the_journal_is_compacted_and_a_restart_keeps_position_and_open_order() {
    let dir = TempDir::new().unwrap();
    let venue = Arc::new(FakeVenue::new());
    let (acks, control) = mpsc::channel(1024);
    let link = Arc::new(AckingLink { control: tokio::sync::Mutex::new(control), acks });
    let limit = 40_000;
    let runtime = Runtime::start(
        RuntimeConfig {
            identity: identity(),
            epoch: 1,
            bootstrap: bootstrap(),
            journal_path: dir.path().join("journal"),
            max_journal_bytes: limit,
            event_queue_capacity: 4096,
            control_url: None,
            events_url: None,
            credential_reference: None,
            link: Some(link),
            session_nonce: "n1".into(),
        },
        venue.clone(),
    )
    .await
    .unwrap();
    runtime
        .set_price("BTCUSDT", colo_runtime::InstrumentPrices::new(Some(100.0), Some(100.0), Some(100.0), Instant::now()))
        .await;
    let id = live_order(&runtime, &venue).await;
    let stream = venue.stream.lock().unwrap().clone().expect("subscribed");
    stream.send(fill(&id, "1", 0.4)).await.unwrap();
    for _ in 0..200 {
        if runtime.position("BTCUSDT").await == 0.4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(runtime.position("BTCUSDT").await, 0.4);

    // Grow the journal well past half its limit with balance updates.
    let balances = vec![AssetBalance { asset: "USDT".into(), free: 1_000.0, locked: 0.0 }];
    let mut peak = 0;
    for _ in 0..300 {
        stream.send(VenueEvent::Balances { balances: balances.clone() }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        peak = peak.max(runtime.journal_bytes().await);
    }
    let mut compacted = false;
    for _ in 0..300 {
        if runtime.journal_bytes().await < limit / 2 {
            compacted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(compacted, "journal never compacted (peak {peak} bytes)");
    assert_eq!(runtime.position("BTCUSDT").await, 0.4);
    runtime.shutdown().await.unwrap();
    drop(runtime);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (restarted, _venue) = start_in(&dir).await;
    assert_eq!(restarted.position("BTCUSDT").await, 0.4);
    assert_eq!(restarted.leaves(&id).await, Some(0.6));
}

/// A runtime whose link records everything it sends to the OMS.
async fn start_linked() -> (Runtime, Arc<FakeVenue>, Arc<FakeLink>, mpsc::Sender<Received<ToRuntime>>, TempDir) {
    let dir = TempDir::new().unwrap();
    let venue = Arc::new(FakeVenue::new());
    let (control_tx, control_rx) = mpsc::channel(64);
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
    (runtime, venue, link, control_tx, dir)
}

/// Kill/enable acknowledgement stages sent so far, in order.
fn ack_stages(link: &FakeLink, command: &str) -> Vec<colo_link::v2::ControlAckStage> {
    link.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.message {
            FromRuntime::ControlAck { command_id, stage, .. } if command_id == command => Some(*stage),
            _ => None,
        })
        .collect()
}

async fn wait_for_stages(link: &FakeLink, command: &str, want: &[colo_link::v2::ControlAckStage]) {
    for _ in 0..400 {
        if ack_stages(link, command) == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("stages for {command}: got {:?}, want {want:?}", ack_stages(link, command));
}

#[tokio::test]
async fn the_link_opens_with_hello_and_recovery_status() {
    let (runtime, _venue, link, _control, _dir) = start_linked().await;
    for _ in 0..200 {
        if link.events.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let sent = link.events.lock().unwrap().clone();
    assert!(matches!(&sent[0].message, FromRuntime::Hello { versions, .. } if versions.contains(&colo_link::v2::PROTOCOL_VERSION)));
    assert!(matches!(&sent[1].message, FromRuntime::RecoveryStatus { ready: true, missing, .. } if missing.is_empty()));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_kill_with_nothing_resting_is_confirmed_clear_at_once() {
    use colo_link::v2::ControlAckStage::*;
    let (runtime, _venue, link, _control, _dir) = start_linked().await;
    grant(&runtime).await;
    kill(&runtime).await.unwrap();
    wait_for_stages(&link, "k", &[Received, AdmissionStopped, CancelsSent, ConfirmedClear]).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_kill_is_confirmed_clear_only_once_the_venue_reports_the_cancel() {
    use colo_link::v2::ControlAckStage::*;
    let (runtime, venue, link, _control, _dir) = start_linked().await;
    let id = live_order(&runtime, &venue).await;
    kill(&runtime).await.unwrap();
    wait_for_stages(&link, "k", &[Received, AdmissionStopped, CancelsSent]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ack_stages(&link, "k").len(), 3, "not clear while the order may rest");

    let stream = venue.stream.lock().unwrap().clone().expect("subscribed");
    stream
        .send(VenueEvent::Order(colo_runtime::RuntimeOrder {
            client_order_id: id,
            venue_order_id: "v-1".into(),
            symbol: "BTCUSDT".into(),
            side: Side::Buy,
            original_quantity: 1.0,
            executed_quantity: 0.0,
            leaves_quantity: 0.0,
            limit_price: Some(100.0),
            state: colo_runtime::RuntimeOrderState::Canceled,
            original_time: chrono::Utc::now(),
        }))
        .await
        .unwrap();
    wait_for_stages(&link, "k", &[Received, AdmissionStopped, CancelsSent, ConfirmedClear]).await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_enable_and_a_policy_are_acknowledged() {
    let (runtime, _venue, link, _control, _dir) = start_linked().await;
    grant(&runtime).await;
    kill(&runtime).await.unwrap();
    runtime
        .apply_control(Received::new(
            ToRuntime::Enable { enable: colo_link::v2::Enable { stopped_epoch: 1, command_id: "e".into() } },
            Instant::now(),
        ))
        .await
        .unwrap();
    wait_for_stages(&link, "e", &[colo_link::v2::ControlAckStage::Received]).await;
    runtime
        .apply_control(Received::new(
            ToRuntime::Policy {
                policy: colo_link::v2::Policy {
                    revision: 2,
                    limits: BTreeMap::new(),
                    trading_state: risk::TradingState::Active,
                    instrument_states: BTreeMap::new(),
                },
            },
            Instant::now(),
        ))
        .await
        .unwrap();
    wait_for("policy ack", || {
        link.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e.message, FromRuntime::PolicyAck { policy_revision: 2 }))
    })
    .await;
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_bootstrap_fee_allowance_holds_back_headroom_on_buys() {
    let dir = TempDir::new().unwrap();
    let mut boot = bootstrap();
    boot.allocation.fee_allowance_bps = 10.0;
    let (runtime, _venue) = start_with(&dir, boot).await;
    grant(&runtime).await;
    // 10 @ 100 spends all 1,000 USDT and leaves nothing for the fee.
    assert!(matches!(runtime.submit(intent(10.0)).await, Err(colo_runtime::RuntimeError::Rejected(_))));
    runtime.submit(intent(9.0)).await.expect("room for the fee");
}

#[tokio::test]
async fn a_fresh_journal_never_reuses_order_ids() {
    let first_dir = TempDir::new().unwrap();
    let (first, _venue) = start_with(&first_dir, bootstrap()).await;
    grant(&first).await;
    let a = first.submit(intent(1.0)).await.unwrap();
    first.shutdown().await.unwrap();

    // Same account, journal lost or a new owner after takeover.
    let second_dir = TempDir::new().unwrap();
    let (second, _venue) = start_with(&second_dir, bootstrap()).await;
    grant(&second).await;
    let b = second.submit(intent(1.0)).await.unwrap();
    assert_eq!(a.sequence, b.sequence, "both journals start counting from the same place");
    assert_ne!(a.client_order_id, b.client_order_id);
    second.shutdown().await.unwrap();
}
