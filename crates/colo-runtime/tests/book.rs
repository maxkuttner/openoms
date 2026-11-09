use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{TimeZone, Utc};
use colo_runtime::{
    AccountIdentity, AssetBalance, Book, InstrumentPrices, InstrumentRules, Intent, Operation,
    OrderType, Prices, RuntimeError, RuntimeOrder, RuntimeOrderState, Side, TimeInForce, Trade,
    TradeKey, TradeProvenance, VenueCapabilities, VenueEvent,
};
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "sub-1".into(),
    }
}

fn rules(symbol: &str, base: &str, quote: &str, tick: f64, step: f64) -> InstrumentRules {
    InstrumentRules {
        symbol: symbol.into(),
        venue: "BINANCE".into(),
        venue_symbol: symbol.into(),
        base_asset: base.into(),
        quote_asset: quote.into(),
        tick_size: tick,
        step_size: step,
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

fn book(limits: BTreeMap<String, risk::Limits>, balances: &[(&str, f64)]) -> Book {
    let mut book = Book::new(identity(), limits).expect("book");
    book.install_instruments([
        rules("BTCUSDT", "BTC", "USDT", 1.0, 1.0),
        rules("ETHUSDT", "ETH", "USDT", 1.0, 1.0),
    ])
    .expect("rules");
    book.install_balances(
        &balances
            .iter()
            .map(|(asset, total)| AssetBalance { asset: (*asset).into(), free: *total, locked: 0.0 })
            .collect::<Vec<_>>(),
    );
    book
}

fn prices(symbol: &str, price: f64) -> Prices {
    let mut prices = Prices::new();
    prices.set(
        symbol,
        InstrumentPrices::new(Some(price), Some(price), Some(price), Instant::now()),
    );
    prices
}

fn intent(symbol: &str, side: Side, quantity: f64, price: f64) -> Intent {
    Intent {
        instrument: symbol.into(),
        side,
        quantity,
        order_type: OrderType::Limit,
        limit_price: Some(price),
        max_quote_spend: None,
        time_in_force: TimeInForce::Gtc,
    }
}

fn trade(identity: &AccountIdentity, order: &colo_runtime::PreparedOrder, id: &str, qty: f64, price: f64) -> Trade {
    Trade {
        key: TradeKey {
            venue: identity.venue.clone(),
            account_id: identity.account_id,
            environment: identity.environment.clone(),
            symbol: order.intent.instrument.clone(),
            trade_id: id.into(),
        },
        venue_order_id: format!("v-{id}"),
        client_order_id: order.submission.client_order_id.clone(),
        side: order.intent.side,
        quantity: qty,
        price,
        executed_at: Utc.timestamp_opt(0, 0).unwrap(),
        fee: None,
        provenance: TradeProvenance::Stream,
    }
}

fn order_event(
    order: &colo_runtime::PreparedOrder,
    state: RuntimeOrderState,
    leaves: f64,
    executed: f64,
) -> VenueEvent {
    VenueEvent::Order(RuntimeOrder {
        client_order_id: order.submission.client_order_id.clone(),
        venue_order_id: "v-1".into(),
        symbol: order.intent.instrument.clone(),
        side: order.intent.side,
        original_quantity: order.intent.quantity,
        executed_quantity: executed,
        leaves_quantity: leaves,
        limit_price: order.intent.limit_price,
        state,
        original_time: Utc.timestamp_opt(0, 0).unwrap(),
    })
}

#[test]
fn cap_ten_admits_first_buy_six_and_rejects_second() {
    let limits = BTreeMap::from([(
        "BTCUSDT".to_string(),
        risk::Limits { max_position_qty: Some(10.0), ..Default::default() },
    )]);
    let mut book = book(limits, &[("USDT", 10_000.0)]);
    let btc = prices("BTCUSDT", 100.0);
    let now = Instant::now();
    let first = book
        .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &btc, now)
        .expect("first buy");
    assert_eq!(first.submission.client_order_id.len(), 36);
    assert!(first.submission.client_order_id.parse::<Uuid>().is_ok());
    let second = book
        .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &btc, now)
        .unwrap_err();
    assert!(matches!(second, RuntimeError::Rejected(r) if r.code == "max_position_qty"));
}

#[test]
fn a_partial_fill_moves_quantity_from_working_to_position_once() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    let events = book.apply(&VenueEvent::Trade(trade(book.identity(), &order, "7", 4.0, 100.0))).expect("fill");
    assert_eq!(events.len(), 1);
    assert_eq!(book.position("BTCUSDT"), 4.0);
    assert_eq!(book.leaves(&order.submission.client_order_id), Some(6.0));
}

#[test]
fn a_duplicate_fill_changes_nothing() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    let fill = VenueEvent::Trade(trade(book.identity(), &order, "7", 4.0, 100.0));
    book.apply(&fill).expect("first fill");
    let duplicate = book.apply(&fill).expect("duplicate");
    assert!(duplicate.is_empty());
    assert_eq!(book.position("BTCUSDT"), 4.0);
    assert_eq!(book.leaves(&order.submission.client_order_id), Some(6.0));
}

#[test]
fn a_cancel_request_keeps_the_reservation_until_terminal_evidence() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    assert_eq!(book.committed("USDT"), 600.0);
    book.request_cancel(&order.submission.client_order_id).expect("cancel");
    assert_eq!(book.committed("USDT"), 600.0);
    book.apply(&VenueEvent::Health {
        state: colo_runtime::StreamState::Stale,
        observed_at: Utc.timestamp_opt(0, 0).unwrap(),
    })
    .expect("health");
    assert_eq!(book.committed("USDT"), 600.0);
}

#[test]
fn a_definitive_reject_releases_the_reservation() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    book.apply(&order_event(&order, RuntimeOrderState::Rejected, 6.0, 0.0))
        .expect("reject");
    assert_eq!(book.committed("USDT"), 0.0);
}

#[test]
fn a_late_fill_after_cancel_still_applies_once() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    book.request_cancel(&order.submission.client_order_id).expect("cancel");
    let fill = VenueEvent::Trade(trade(book.identity(), &order, "8", 4.0, 100.0));
    book.apply(&fill).expect("late fill");
    book.apply(&fill).expect("duplicate late fill");
    assert_eq!(book.position("BTCUSDT"), 4.0);
    book.apply(&order_event(&order, RuntimeOrderState::Canceled, 6.0, 4.0))
        .expect("cancel terminal");
    assert_eq!(book.position("BTCUSDT"), 4.0);
    assert_eq!(book.committed("USDT"), 0.0);
}

#[test]
fn a_replacement_candidate_charges_overlap_until_the_old_one_is_terminal() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    let btc = prices("BTCUSDT", 100.0);
    let now = Instant::now();
    let original = book
        .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &btc, now)
        .expect("original");
    let replacement = book
        .prepare(intent("BTCUSDT", Side::Buy, 3.0, 100.0), &btc, now)
        .expect("replacement candidate");
    assert_eq!(book.committed("USDT"), 900.0);
    book.apply(&order_event(&original, RuntimeOrderState::Canceled, 6.0, 0.0))
        .expect("old terminal");
    assert_eq!(book.committed("USDT"), 300.0);
    assert_eq!(book.leaves(&replacement.submission.client_order_id), Some(3.0));
}

#[test]
fn reducing_capacity_cannot_cross_flat() {
    let mut book = book(BTreeMap::new(), &[("BTC", 100.0)]);
    book.install_position("BTCUSDT", 5.0).expect("baseline");
    book.set_trading_state_reducing();
    let err = book
        .prepare(intent("BTCUSDT", Side::Sell, 6.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::Rejected(r) if r.code == "trading_reducing"));
}

#[test]
fn two_symbols_share_the_quote_balance() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    let btc = prices("BTCUSDT", 100.0);
    let eth = prices("ETHUSDT", 100.0);
    let now = Instant::now();
    book.prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &btc, now)
        .expect("BTC spend");
    let err = book
        .prepare(intent("ETHUSDT", Side::Buy, 6.0, 100.0), &eth, now)
        .unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::InsufficientAssets { asset, required: 600.0, available: 400.0 } if asset == "USDT"
    ));
}

#[test]
fn a_fee_allowance_prevents_spending_the_last_funds() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    book.set_fee_allowance_bps(10.0).expect("fee");
    let err = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InsufficientAssets { asset, .. } if asset == "USDT"));
}

#[test]
fn a_free_plus_locked_snapshot_does_not_charge_the_same_order_twice() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    let btc = prices("BTCUSDT", 100.0);
    let now = Instant::now();
    book.prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &btc, now)
        .expect("first");
    book.apply(&VenueEvent::Balances {
        balances: vec![AssetBalance { asset: "USDT".into(), free: 400.0, locked: 600.0 }],
    })
    .expect("snapshot");
    assert_eq!(book.committed("USDT"), 600.0);
    assert_eq!(book.free("USDT"), 400.0);
    book.prepare(intent("BTCUSDT", Side::Buy, 4.0, 100.0), &btc, now)
        .expect("remaining funds");
}

#[test]
fn stale_market_data_is_rejected() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    book.set_max_price_age(std::time::Duration::from_secs(1));
    let mut stale = Prices::new();
    stale.set(
        "BTCUSDT",
        InstrumentPrices::new(Some(100.0), Some(100.0), Some(100.0), Instant::now() - std::time::Duration::from_secs(2)),
    );
    let err = book
        .prepare(intent("BTCUSDT", Side::Buy, 1.0, 100.0), &stale, Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidIntent(msg) if msg.contains("older")));
}

#[test]
fn unbounded_market_spend_is_rejected() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    let mut market = intent("BTCUSDT", Side::Buy, 1.0, 100.0);
    market.order_type = OrderType::Market;
    market.limit_price = None;
    let err = book.prepare(market, &prices("BTCUSDT", 100.0), Instant::now()).unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidIntent(_)));
}

#[test]
fn precision_violations_are_rejected_not_rounded_down() {
    let mut precise = Book::new(identity(), BTreeMap::new()).expect("book");
    precise
        .install_instruments([rules("BTCUSDT", "BTC", "USDT", 1.0, 1.0)])
        .expect("rules");
    precise.install_balances(&[AssetBalance { asset: "USDT".into(), free: 10_000.0, locked: 0.0 }]);
    let err = precise
        .prepare(intent("BTCUSDT", Side::Buy, 1.5, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidIntent(msg) if msg.contains("step")));
    let err = precise
        .prepare(intent("BTCUSDT", Side::Buy, 2.0, 100.5), &prices("BTCUSDT", 100.5), Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InvalidIntent(msg) if msg.contains("tick")));
    precise
        .prepare(intent("BTCUSDT", Side::Buy, 2.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("aligned boundary");
}

#[tokio::test]
async fn concurrent_callers_share_one_serialized_owner() {
    let limits = BTreeMap::from([(
        "BTCUSDT".to_string(),
        risk::Limits { max_position_qty: Some(10.0), ..Default::default() },
    )]);
    let book = Arc::new(tokio::sync::Mutex::new(book(limits, &[("USDT", 10_000.0)])));
    let run = |book: Arc<tokio::sync::Mutex<Book>>| async move {
        let mut guard = book.lock().await;
        guard
            .prepare(intent("BTCUSDT", Side::Buy, 6.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
            .map(|_| ())
    };
    let (a, b) = tokio::join!(run(book.clone()), run(book.clone()));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let guard = book.lock().await;
    assert_eq!(guard.committed("USDT"), 600.0);
    assert!(guard.committed("USDT") <= 1_000.0);
}

#[test]
fn risk_rejections_do_not_consume_the_rate_window() {
    let limits = BTreeMap::from([(
        "BTCUSDT".to_string(),
        risk::Limits {
            max_position_qty: Some(2.0),
            max_orders_per_sec: Some(1),
            ..Default::default()
        },
    )]);
    let mut book = book(limits, &[("USDT", 10_000.0)]);
    let btc = prices("BTCUSDT", 100.0);
    let now = Instant::now();
    let rejected = book.prepare(intent("BTCUSDT", Side::Buy, 3.0, 100.0), &btc, now);
    assert!(matches!(rejected, Err(RuntimeError::Rejected(_))));
    book.prepare(intent("BTCUSDT", Side::Buy, 1.0, 100.0), &btc, now)
        .expect("the risk rejection did not occupy the only slot");
    let limited = book.prepare(intent("BTCUSDT", Side::Buy, 1.0, 100.0), &btc, now);
    assert!(matches!(limited, Err(RuntimeError::RateLimited { limit: 1 })));
}

#[test]
fn an_unknown_external_order_pauses_admission_until_recovery() {
    let mut book = book(BTreeMap::new(), &[("USDT", 1_000.0)]);
    let external = VenueEvent::Order(RuntimeOrder {
        client_order_id: "not-ours".into(),
        venue_order_id: "v-9".into(),
        symbol: "BTCUSDT".into(),
        side: Side::Buy,
        original_quantity: 1.0,
        executed_quantity: 0.0,
        leaves_quantity: 1.0,
        limit_price: Some(100.0),
        state: RuntimeOrderState::New,
        original_time: Utc.timestamp_opt(0, 0).unwrap(),
    });
    assert!(matches!(book.apply(&external), Err(RuntimeError::IncompleteRecovery(_))));
    assert!(book.is_paused());
    let err = book
        .prepare(intent("BTCUSDT", Side::Buy, 1.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .unwrap_err();
    assert!(matches!(err, RuntimeError::IncompleteRecovery(_)));
    book.resume_after_recovery();
    book.prepare(intent("BTCUSDT", Side::Buy, 1.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("admission resumes only after recovery");
}

/// A fresh book for the same account, as a restart builds before restoring.
fn empty_book_like(original: &Book) -> Book {
    let mut book = Book::new(original.identity().clone(), BTreeMap::new()).expect("book");
    book.install_instruments([
        rules("BTCUSDT", "BTC", "USDT", 1.0, 1.0),
        rules("ETHUSDT", "ETH", "USDT", 1.0, 1.0),
    ])
    .expect("rules");
    book
}

#[test]
fn a_checkpoint_restores_positions_open_orders_and_their_reservations() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    book.apply(&VenueEvent::Trade(trade(book.identity(), &order, "7", 4.0, 100.0))).expect("fill");
    let checkpoint = book.checkpoint();

    let mut restored = empty_book_like(&book);
    restored.restore_checkpoint(&checkpoint).expect("restore");
    let id = &order.submission.client_order_id;
    assert_eq!(restored.position("BTCUSDT"), 4.0);
    assert_eq!(restored.leaves(id), Some(6.0));
    assert_eq!(restored.committed("USDT"), book.committed("USDT"));
    assert_eq!(restored.free("USDT"), book.free("USDT"));
    assert_eq!(restored.sequence(), book.sequence());
}

#[test]
fn a_fill_counted_before_the_checkpoint_is_not_counted_again() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    let fill = VenueEvent::Trade(trade(book.identity(), &order, "7", 4.0, 100.0));
    book.apply(&fill).expect("fill");
    let mut restored = empty_book_like(&book);
    restored.restore_checkpoint(&book.checkpoint()).expect("restore");
    assert!(restored.apply(&fill).expect("redelivered").is_empty());
    assert_eq!(restored.position("BTCUSDT"), 4.0);
}

#[test]
fn a_checkpoint_drops_finished_orders_but_keeps_their_position() {
    let mut book = book(BTreeMap::new(), &[("USDT", 10_000.0)]);
    let order = book
        .prepare(intent("BTCUSDT", Side::Buy, 10.0, 100.0), &prices("BTCUSDT", 100.0), Instant::now())
        .expect("prepare");
    book.apply(&VenueEvent::Trade(trade(book.identity(), &order, "8", 10.0, 100.0))).expect("fill");
    let mut restored = empty_book_like(&book);
    restored.restore_checkpoint(&book.checkpoint()).expect("restore");
    assert_eq!(restored.position("BTCUSDT"), 10.0);
    assert_eq!(restored.leaves(&order.submission.client_order_id), None);
    assert_eq!(restored.committed("USDT"), 0.0);
}
