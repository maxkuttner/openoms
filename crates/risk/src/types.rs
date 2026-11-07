use serde::{Deserialize, Serialize};

/// Quote currencies strategy-wide notional defaults are read in. A per-scope
/// override may use any quote; an inherited default may not.
pub const USD_LIKE: [&str; 3] = ["USD", "USDT", "USDC"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingState {
    Active,
    Reducing,
    Halted,
}

/// Per-instrument limits. Every field is optional: `None` means the check is
/// not configured. Rate limiting is intentionally absent from the evaluator;
/// the caller owns the clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub max_order_qty: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_qty: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub price_band_pct: Option<f64>,
    pub max_orders_per_sec: Option<u32>,
}

/// Signed filled position plus the still-working quantity on each side.
/// Opposing working orders are never netted: each side reserves its own
/// capacity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    pub position_qty: f64,
    pub working_buy_qty: f64,
    pub working_sell_qty: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderInput {
    pub side: Side,
    pub quantity: f64,
    /// Limit price for a limit order; `None` for a market order.
    pub limit_price: Option<f64>,
    /// Contract multiplier, applied exactly once to notional calculations.
    pub multiplier: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Valuation {
    /// Price used to value the candidate order when it has no limit price.
    pub order_price: Option<f64>,
    /// Price used to value the projected position. Never silently replaced by
    /// the candidate order's price.
    pub position_price: Option<f64>,
    /// Reference (mark) price for the price band.
    pub reference_price: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RiskInput {
    pub order: OrderInput,
    pub exposure: Exposure,
    pub limits: Limits,
    pub state: TradingState,
    pub valuation: Valuation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub code: String,
    pub message: String,
}

impl Reject {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Reject {}
