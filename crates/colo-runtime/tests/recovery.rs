use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use colo_link::v2::{Allocation, Bootstrap, RuntimeCheckpoint, Timings};
use colo_runtime::{
    AccountIdentity, AssetBalance, InstrumentRules, Intent, Journal, JournalRecord, Operation,
    OrderType, Reservation, RuntimeOrder, RuntimeOrderState, Side, Submission, TimeInForce, Trade,
    TradeKey, TradeProvenance, VenueCapabilities, VenueEvent,
};
use gateway::adapter::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};
use gateway::contract::{BrokerOpenOrder, BrokerOrderState};
use gateway::runtime::{ExecutionCursor, ExecutionPage, RuntimeVenue, SymbolCursor};
use tempfile::TempDir;
use tokio::sync::mpsc;
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

fn bootstrap(assets: &[(&str, f64)], killed: Option<u64>) -> Bootstrap {
    let now = Utc.timestamp_opt(0, 0).unwrap();
    let mut cursor = ExecutionCursor::default();
    cursor.positions.insert(
        "BTCUSDT".into(),
        SymbolCursor { last_trade_id: None, start_time: Some(now) },
    );
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
            execution_cursor: cursor,
            complete: true,
        },
        asset_baseline: assets.iter().map(|(a, q)| ((*a).into(), *q)).collect(),
        killed_epoch: killed,
        allocation: Allocation { quote_asset: "USDT".into(), max_notional: 10_000.0, max_position_qty: Some(10.0) },
        recovery_universe: vec!["BTCUSDT".into()],
    }
}

struct FakeVenue {
    orders: Mutex<Vec<RuntimeOrder>>,
    balances: Mutex<Vec<AssetBalance>>,
    pages: Mutex<VecDeque<ExecutionPage>>,
    stream: Mutex<Vec<VenueEvent>>,
    fail_orders: bool,
}

impl FakeVenue {
    fn new(orders: Vec<RuntimeOrder>, balances: Vec<AssetBalance>) -> Self {
        Self {
            orders: Mutex::new(orders),
            balances: Mutex::new(balances),
            pages: Mutex::new(VecDeque::new()),
            stream: Mutex::new(Vec::new()),
            fail_orders: false,
        }
    }

    fn with_page(mut self, page: ExecutionPage) -> Self {
        self.pages.get_mut().unwrap().push_back(page);
        self
    }

    fn with_stream(self, event: VenueEvent) -> Self {
        self.stream.lock().unwrap().push(event);
        self
    }

    fn failing(mut self) -> Self {
        self.fail_orders = true;
        self
    }
}

#[async_trait]
impl BrokerAdapter for FakeVenue {
    async fn submit_order(&self, _: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        Err(BrokerError::NotConfigured("fake".into()))
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> { Ok(()) }
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> { Ok(Vec::new()) }
    async fn order_status(&self, _: &str, _: &str, _: &str) -> Result<BrokerOrderState, BrokerError> {
        Err(BrokerError::NoSuchOrder("fake".into()))
    }
}

#[async_trait]
impl RuntimeVenue for FakeVenue {
    async fn metadata(&self, _: &[String]) -> Result<Vec<InstrumentRules>, BrokerError> { Ok(vec![rules()]) }
    async fn balances(&self) -> Result<Vec<AssetBalance>, BrokerError> { Ok(self.balances.lock().unwrap().clone()) }
    async fn snapshot_orders(&self) -> Result<Vec<RuntimeOrder>, BrokerError> {
        if self.fail_orders { return Err(BrokerError::Network("snapshot down".into())); }
        Ok(self.orders.lock().unwrap().clone())
    }
    async fn executions(&self, cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError> {
        Ok(self.pages.lock().unwrap().pop_front().unwrap_or(ExecutionPage {
            trades: vec![],
            next: cursor.clone(),
            complete: true,
            missing: vec![],
        }))
    }
    async fn lookup(&self, _: &str, _: &str) -> Result<RuntimeOrder, BrokerError> {
        Err(BrokerError::NoSuchOrder("absent".into()))
    }
    async fn subscribe(&self, tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError> {
        let events: Vec<VenueEvent> = self.stream.lock().unwrap().drain(..).collect();
        for event in events {
            let _ = tx.send(event).await;
        }
        Ok(())
    }
}

fn order(client_id: &str, leaves: f64, state: RuntimeOrderState) -> RuntimeOrder {
    RuntimeOrder {
        client_order_id: client_id.into(),
        venue_order_id: "v-1".into(),
        symbol: "BTCUSDT".into(),
        side: Side::Buy,
        original_quantity: 10.0,
        executed_quantity: 0.0,
        leaves_quantity: leaves,
        limit_price: Some(100.0),
        state,
        original_time: Utc.timestamp_opt(0, 0).unwrap(),
    }
}

fn trade(identity: &AccountIdentity, client_id: &str, id: &str, qty: f64) -> Trade {
    Trade {
        key: TradeKey {
            venue: identity.venue.clone(),
            account_id: identity.account_id,
            environment: identity.environment.clone(),
            symbol: "BTCUSDT".into(),
            trade_id: id.into(),
        },
        venue_order_id: "v-1".into(),
        client_order_id: client_id.into(),
        side: Side::Buy,
        quantity: qty,
        price: 100.0,
        executed_at: Utc.timestamp_opt(0, 0).unwrap(),
        fee: None,
        provenance: TradeProvenance::History,
    }
}

fn journal_with_intent(dir: &TempDir, id: &str) -> (Journal, AccountIdentity) {
    let identity = identity();
    let path = dir.path().join("journal");
    let mut journal = Journal::open(&path, &identity).unwrap();
    journal
        .append_durable(&JournalRecord::Intent {
            submission: Submission { client_order_id: id.into(), sequence: 1 },
            intent: Intent {
                instrument: "BTCUSDT".into(),
                side: Side::Buy,
                quantity: 10.0,
                order_type: OrderType::Limit,
                limit_price: Some(100.0),
                max_quote_spend: None,
                time_in_force: TimeInForce::Gtc,
            },
            reservation: Reservation { asset: "USDT".into(), amount: 1_000.0 },
        })
        .unwrap();
    (journal, identity)
}

fn balances() -> Vec<AssetBalance> {
    vec![AssetBalance { asset: "USDT".into(), free: 1_000.0, locked: 0.0 }]
}

#[tokio::test]
async fn restart_restores_an_open_order_and_reserves_again() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], balances());
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await.unwrap();
    assert!(recovered.complete);
    assert_eq!(recovered.book.leaves("c-1"), Some(10.0));
    assert_eq!(recovered.book.committed("USDT"), 1_000.0);
}

#[tokio::test]
async fn a_fill_between_stream_setup_and_snapshot_is_applied_once() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], balances())
        .with_stream(VenueEvent::Trade(trade(journal.identity(), "c-1", "7", 4.0)));
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await.unwrap();
    assert_eq!(recovered.book.position("BTCUSDT"), 4.0);
    assert_eq!(recovered.book.leaves("c-1"), Some(6.0));
}

#[tokio::test]
async fn duplicate_history_and_stream_fills_move_the_position_once() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let page = ExecutionPage {
        trades: vec![trade(journal.identity(), "c-1", "7", 4.0)],
        next: ExecutionCursor::default(),
        complete: true,
        missing: vec![],
    };
    let venue = FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], balances())
        .with_page(page)
        .with_stream(VenueEvent::Trade(trade(journal.identity(), "c-1", "7", 4.0)));
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await.unwrap();
    assert_eq!(recovered.book.position("BTCUSDT"), 4.0);
}

#[tokio::test]
async fn fully_closed_history_without_open_orders_still_recovers() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let page = ExecutionPage {
        trades: vec![trade(journal.identity(), "c-1", "8", 10.0)],
        next: ExecutionCursor::default(),
        complete: true,
        missing: vec![],
    };
    let venue = FakeVenue::new(vec![], balances()).with_page(page);
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await.unwrap();
    assert_eq!(recovered.book.position("BTCUSDT"), 10.0);
    assert_eq!(recovered.book.leaves("c-1"), Some(0.0));
    assert!(recovered.complete);
}

#[tokio::test]
async fn incomplete_history_is_a_blocking_error_not_a_flat_account() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let page = ExecutionPage {
        trades: vec![],
        next: ExecutionCursor::default(),
        complete: false,
        missing: vec!["BTCUSDT".into()],
    };
    let venue = FakeVenue::new(vec![], balances()).with_page(page);
    let result = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await;
    assert!(matches!(result, Err(colo_runtime::RuntimeError::IncompleteRecovery(_))));
}

#[tokio::test]
async fn a_failed_snapshot_is_not_an_empty_account() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![], balances()).failing();
    let result = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await;
    assert!(matches!(result, Err(colo_runtime::RuntimeError::IncompleteRecovery(_))));
}

#[tokio::test]
async fn wrong_inventory_blocks_recovery() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], vec![
        AssetBalance { asset: "USDT".into(), free: 900.0, locked: 0.0 },
    ]);
    let result = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await;
    assert!(matches!(result, Err(colo_runtime::RuntimeError::IncompleteRecovery(_))));
}

#[tokio::test]
async fn an_unresolved_pending_order_is_evidence_not_ready() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![], balances());
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], None)).await.unwrap();
    assert!(!recovered.complete);
    assert!(recovered.evidence.iter().any(|e| e.contains("c-1")));
}

#[tokio::test]
async fn a_retained_kill_latch_blocks_readiness() {
    let dir = TempDir::new().unwrap();
    let (mut journal, _) = journal_with_intent(&dir, "c-1");
    let venue = FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], balances());
    let recovered = colo_runtime::recover(&venue, &mut journal, &bootstrap(&[("USDT", 1_000.0)], Some(3))).await.unwrap();
    assert!(!recovered.complete);
    assert!(recovered.evidence.iter().any(|e| e.contains("kill latch")));
}

fn history_fill(identity: &AccountIdentity, id: &str, qty: f64) -> ExecutionPage {
    ExecutionPage {
        trades: vec![trade(identity, "c-1", id, qty)],
        next: ExecutionCursor::default(),
        complete: true,
        missing: vec![],
    }
}

fn open_order_venue() -> FakeVenue {
    FakeVenue::new(vec![order("c-1", 10.0, RuntimeOrderState::New)], balances())
}

#[tokio::test]
async fn a_fill_found_in_history_survives_the_next_restart() {
    let dir = TempDir::new().unwrap();
    let (mut journal, identity) = journal_with_intent(&dir, "c-1");
    let boot = bootstrap(&[("USDT", 1_000.0)], None);
    let venue = open_order_venue().with_page(history_fill(&identity, "7", 4.0));
    let first = colo_runtime::recover(&venue, &mut journal, &boot).await.unwrap();
    assert_eq!(first.book.position("BTCUSDT"), 4.0);
    drop(first);

    // The venue's history has moved on: the fill is not served again.
    let second = colo_runtime::recover(&open_order_venue(), &mut journal, &boot).await.unwrap();
    assert_eq!(second.book.position("BTCUSDT"), 4.0, "the fill was only in memory");
    assert_eq!(second.book.leaves("c-1"), Some(6.0));
}

#[tokio::test]
async fn a_restart_after_compaction_keeps_positions_and_open_orders() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("journal");
    let (mut journal, identity) = journal_with_intent(&dir, "c-1");
    let boot = bootstrap(&[("USDT", 1_000.0)], None);
    let venue = open_order_venue().with_page(history_fill(&identity, "7", 4.0));
    let first = colo_runtime::recover(&venue, &mut journal, &boot).await.unwrap();
    let committed = first.book.committed("USDT");
    drop(first);
    journal.ack_through(journal.last_sequence()).unwrap();
    journal.compact().unwrap();
    assert!(!journal.replay().unwrap().iter().any(|r| matches!(r.record, JournalRecord::Intent { .. })));
    drop(journal);

    let mut journal = Journal::open(&path, &identity).unwrap();
    let second = colo_runtime::recover(&open_order_venue(), &mut journal, &boot).await.unwrap();
    assert!(second.complete, "{:?}", second.evidence);
    assert_eq!(second.book.position("BTCUSDT"), 4.0);
    assert_eq!(second.book.leaves("c-1"), Some(6.0));
    assert_eq!(second.book.committed("USDT"), committed);
}

#[tokio::test]
async fn records_after_the_checkpoint_are_applied_once() {
    let dir = TempDir::new().unwrap();
    let (mut journal, identity) = journal_with_intent(&dir, "c-1");
    let boot = bootstrap(&[("USDT", 1_000.0)], None);
    let venue = open_order_venue().with_page(history_fill(&identity, "7", 4.0));
    drop(colo_runtime::recover(&venue, &mut journal, &boot).await.unwrap());
    journal
        .append_durable(&JournalRecord::Venue { event: VenueEvent::Trade(trade(&identity, "c-1", "8", 2.0)) })
        .unwrap();
    let second = colo_runtime::recover(&open_order_venue(), &mut journal, &boot).await.unwrap();
    assert_eq!(second.book.position("BTCUSDT"), 6.0);
    assert_eq!(second.book.leaves("c-1"), Some(4.0));
}
