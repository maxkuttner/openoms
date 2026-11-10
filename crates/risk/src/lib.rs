//! Pure pre-trade risk rules shared by the OMS and the embedded co-located
//! runtime. No I/O, no clock, no storage: callers own rate windows,
//! reservations, and persistence.

mod evaluate;
mod types;

pub use evaluate::{evaluate, require_prices, resolve_limits};
pub use types::{
    Exposure, Limits, OrderInput, Reject, RiskInput, Side, TradingState, Valuation, VenueRules, USD_LIKE,
};

#[cfg(test)]
mod tests;
