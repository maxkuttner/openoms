//! Pre-trade checks a co-located strategy runs before every order: the limits
//! the OMS holds for it, a price band and an orders-per-second cap. Pure and
//! in memory — nothing here waits on I/O, so it sits on the order path.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Quote currencies notional limits are read in. Any other quote needs its
/// own per-symbol limit (the OMS's rule, phase 7).
pub const USD_LIKE: [&str; 3] = ["USD", "USDT", "USDC"];

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    pub max_order_qty: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_qty: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>,
    pub max_orders_per_sec: Option<u32>,
}

pub struct OrderIntent<'a> {
    pub symbol: &'a str,
    pub quote_currency: &'a str,
    pub buy: bool,
    pub qty: f64,
    pub price: f64,
}

#[derive(Debug, PartialEq)]
pub struct Reject {
    pub code: &'static str,
    pub message: String,
}

pub struct RiskBook {
    limits: Limits,
    version: u64,
    positions: HashMap<String, f64>,
    halted: bool,
    window: VecDeque<Instant>,
}

impl RiskBook {
    pub fn new(limits: Limits, version: u64) -> Self {
        Self { limits, version, positions: HashMap::new(), halted: false, window: VecDeque::new() }
    }
    pub fn set_limits(&mut self, limits: Limits, version: u64) {
        self.limits = limits;
        self.version = version;
    }

    /// The limits version this book enforces (what the strategy acknowledges).
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Replace the positions with the OMS's booked ones.
    pub fn set_positions(&mut self, positions: impl IntoIterator<Item = (String, f64)>) {
        self.positions = positions.into_iter().collect();
    }

    pub fn on_fill(&mut self, symbol: &str, buy: bool, qty: f64) {
        *self.positions.entry(symbol.to_string()).or_insert(0.0) += if buy { qty } else { -qty };
    }

    pub fn halt(&mut self) {
        self.halted = true;
    }

    pub fn resume(&mut self) {
        self.halted = false;
    }

    pub fn halted(&self) -> bool {
        self.halted
    }

    /// Check one order against everything; `reference` is the strategy's own
    /// mid. A passing order is counted in the rate window.
    pub fn check(&mut self, o: &OrderIntent, reference: f64, now: Instant) -> Result<(), Reject> {
        let reject = |code: &'static str, message: String| Err(Reject { code, message });
        if self.halted {
            return reject("halted", "trading is halted".into());
        }
        let l = &self.limits;
        if let Some(max) = l.max_order_qty {
            if o.qty > max {
                return reject("max_order_qty", format!("order qty {} > {max}", o.qty));
            }
        }
        let has_notional = l.max_order_notional.is_some() || l.max_position_notional.is_some();
        if has_notional && !USD_LIKE.contains(&o.quote_currency) {
            return reject("no_notional_limit", format!("no notional limit for quote currency {}", o.quote_currency));
        }
        if let Some(max) = l.max_order_notional {
            let notional = o.qty * o.price;
            if notional > max {
                return reject("max_order_notional", format!("order notional {notional} > {max}"));
            }
        }
        let position = self.positions.get(o.symbol).copied().unwrap_or(0.0);
        let projected = position + if o.buy { o.qty } else { -o.qty };
        if let Some(max) = l.max_position_qty {
            if projected.abs() > max {
                return reject("max_position_qty", format!("projected position {projected} > {max}"));
            }
        }
        if let Some(max) = l.max_position_notional {
            let notional = projected.abs() * o.price;
            if notional > max {
                return reject("max_position_notional", format!("projected position notional {notional} > {max}"));
            }
        }
        if let Some(band) = l.price_band_pct {
            let off = (o.price - reference).abs() / reference * 100.0;
            if off > band * (1.0 + 1e-9) {
                return reject("price_band", format!("price {} is {off:.2}% from {reference} (band {band}%)", o.price));
            }
        }
        if let Some(cap) = l.max_orders_per_sec {
            while self.window.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
                self.window.pop_front();
            }
            if self.window.len() >= cap as usize {
                return reject("order_rate", format!("more than {cap} orders in the last second"));
            }
            self.window.push_back(now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(qty: f64, price: f64) -> OrderIntent<'static> {
        OrderIntent { symbol: "BTCUSDT", quote_currency: "USDT", buy: true, qty, price }
    }

    fn code(r: Result<(), Reject>) -> &'static str {
        r.err().map(|e| e.code).unwrap_or("ok")
    }

    #[test]
    fn a_halted_book_rejects_everything() {
        let mut b = RiskBook::new(Limits::default(), 1);
        b.halt();
        assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, Instant::now())), "halted");
        b.resume();
        assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, Instant::now())), "ok");
    }

    #[test]
    fn order_and_position_limits_reject_with_their_code() {
        let now = Instant::now();
        let mut b = RiskBook::new(Limits { max_order_qty: Some(2.0), ..Default::default() }, 1);
        assert_eq!(code(b.check(&order(3.0, 100.0), 100.0, now)), "max_order_qty");
        let mut b = RiskBook::new(Limits { max_order_notional: Some(150.0), ..Default::default() }, 1);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, now)), "max_order_notional");
        let mut b = RiskBook::new(Limits { max_position_qty: Some(5.0), ..Default::default() }, 1);
        b.set_positions([("BTCUSDT".to_string(), 4.0)]);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, now)), "max_position_qty");
        let mut b = RiskBook::new(Limits { max_position_notional: Some(500.0), ..Default::default() }, 1);
        b.set_positions([("BTCUSDT".to_string(), 4.0)]);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, now)), "max_position_notional");
    }

    #[test]
    fn notional_limits_need_a_usd_like_quote() {
        let mut b = RiskBook::new(Limits { max_order_notional: Some(1_000.0), ..Default::default() }, 1);
        let eth_btc = OrderIntent { symbol: "ETHBTC", quote_currency: "BTC", buy: true, qty: 1.0, price: 0.05 };
        assert_eq!(code(b.check(&eth_btc, 0.05, Instant::now())), "no_notional_limit");
        assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, Instant::now())), "ok");
    }

    #[test]
    fn the_band_rejects_a_price_too_far_from_reference() {
        let mut b = RiskBook::new(Limits { price_band_pct: Some(5.0), ..Default::default() }, 1);
        assert_eq!(code(b.check(&order(1.0, 104.0), 100.0, Instant::now())), "ok");
        assert_eq!(code(b.check(&order(1.0, 106.0), 100.0, Instant::now())), "price_band");
        assert_eq!(code(b.check(&order(1.0, 95.0), 100.0, Instant::now())), "ok");
    }

    #[test]
    fn the_order_rate_cap_counts_a_trailing_second() {
        let mut b = RiskBook::new(Limits { max_orders_per_sec: Some(3), ..Default::default() }, 1);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, t0)), "ok");
        }
        assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, t0 + Duration::from_millis(500))), "order_rate");
        assert_eq!(code(b.check(&order(1.0, 100.0), 100.0, t0 + Duration::from_millis(1001))), "ok");
    }

    #[test]
    fn fills_move_the_position_the_limits_see() {
        let mut b = RiskBook::new(Limits { max_position_qty: Some(3.0), ..Default::default() }, 1);
        b.on_fill("BTCUSDT", true, 2.0);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, Instant::now())), "max_position_qty");
        b.on_fill("BTCUSDT", false, 2.0);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, Instant::now())), "ok");
    }

    #[test]
    fn new_limits_replace_the_old_and_carry_their_version() {
        let mut b = RiskBook::new(Limits::default(), 1);
        b.set_limits(Limits { max_order_qty: Some(1.0), ..Default::default() }, 2);
        assert_eq!(b.version(), 2);
        assert_eq!(code(b.check(&order(2.0, 100.0), 100.0, Instant::now())), "max_order_qty");
    }
}
