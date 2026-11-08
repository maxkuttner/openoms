//! In-process admission latency evidence. This measures local preparation and
//! reservation only; it deliberately excludes remote venue latency.
//!
//! cargo run --release -p colo-runtime --example latency -- [samples]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::Utc;
use colo_runtime::{
    AccountIdentity, AssetBalance, Book, InstrumentPrices, InstrumentRules, Intent, Operation,
    OrderType, Prices, RuntimeOrder, RuntimeOrderState, Side, TimeInForce, VenueCapabilities,
    VenueEvent,
};
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "bench".into(),
    }
}

fn rules() -> InstrumentRules {
    InstrumentRules {
        symbol: "BTCUSDT".into(),
        venue: "BINANCE".into(),
        venue_symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        tick_size: 0.01,
        step_size: 0.00001,
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

fn main() {
    let samples: usize = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(2_000);
    let mut book = Book::new(
        identity(),
        BTreeMap::from([("BTCUSDT".to_string(), risk::Limits::default())]),
    )
    .expect("book");
    book.install_instruments([rules()]).expect("rules");
    book.install_balances(&[AssetBalance { asset: "USDT".into(), free: 1e12, locked: 0.0 }]);

    let mut latencies = Vec::with_capacity(samples);
    let mut errors = 0usize;
    for _ in 0..samples {
        let mut prices = Prices::new();
        prices.set(
            "BTCUSDT",
            InstrumentPrices::new(Some(64_000.0), Some(64_000.0), Some(64_000.0), Instant::now()),
        );
        let start = Instant::now();
        let prepared = book.prepare(
            Intent {
                instrument: "BTCUSDT".into(),
                side: Side::Buy,
                quantity: 0.00001,
                order_type: OrderType::Limit,
                limit_price: Some(64_000.0),
                max_quote_spend: None,
                time_in_force: TimeInForce::Gtc,
            },
            &prices,
            start,
        );
        let elapsed = start.elapsed();
        match prepared {
            Ok(prepared) => {
                latencies.push(elapsed);
                let order = RuntimeOrder {
                    client_order_id: prepared.submission.client_order_id,
                    venue_order_id: "bench".into(),
                    symbol: "BTCUSDT".into(),
                    side: Side::Buy,
                    original_quantity: 0.00001,
                    executed_quantity: 0.0,
                    leaves_quantity: 0.0,
                    limit_price: Some(64_000.0),
                    state: RuntimeOrderState::Rejected,
                    original_time: Utc::now(),
                };
                let _ = book.apply(&VenueEvent::Order(order));
            }
            Err(_) => errors += 1,
        }
    }
    latencies.sort_unstable();
    let pct = |p: f64| -> Duration {
        if latencies.is_empty() {
            return Duration::ZERO;
        }
        let index = ((latencies.len() - 1) as f64 * p).round() as usize;
        latencies[index]
    };
    let p50 = pct(0.50).as_nanos();
    let p99 = pct(0.99).as_nanos();
    let p999 = pct(0.999).as_nanos();
    let max = latencies.last().copied().unwrap_or_default().as_nanos();
    println!(
        "{{\"kind\":\"colo-admission-latency\",\"samples\":{},\"errors\":{},\"p50_ns\":{},\"p99_ns\":{},\"p999_ns\":{},\"max_ns\":{},\"arch\":\"{}\",\"os\":\"{}\",\"journal\":\"excluded\",\"durability\":\"local-only\",\"venue_latency\":\"excluded\"}}",
        latencies.len(),
        errors,
        p50,
        p99,
        p999,
        max,
        std::env::consts::ARCH,
        std::env::consts::OS,
    );
}
