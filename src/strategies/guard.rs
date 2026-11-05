//! The strategy rules inside the one order path (`order_entry::submit`).
//!
//! A strategy's orders get every check a cockpit order gets, plus:
//! - only the strategy's own principal may trade its portfolio;
//! - the strategy's state (Halted admits nothing; Reducing only reductions);
//! - its limits, per instrument, with a `risk_limits` row overriding them;
//!   strategy-wide notional defaults only price USD-like quotes;
//! - a price band around the live mark, and market-order notional from it.
//!
//! The mark is looked up before the order's transaction, so waiting for a first
//! quote never holds the strategy's row lock.

use std::time::Duration;

use sqlx::PgConnection;
use tracing::warn;
use uuid::Uuid;

use dataprovider::InstrumentRef;
use gateway::md::MdHandle;

use crate::domain::orders::commands::SubmitOrder;
use crate::domain::orders::risk::{check_limit, check_trading_state, RiskLimits, RiskRejection};
use crate::domain::orders::state::OrderSide;
use crate::marks::{Mark, MarkStore};
use crate::risk_engine::{PgRiskDataProvider, RiskDataProvider, RiskScope};

use super::Strategy;

/// Quote currencies the strategy-wide notional defaults are read in.
pub const USD_LIKE: [&str; 3] = ["USD", "USDT", "USDC"];

/// How long an order waits for a first quote on an instrument nobody priced yet.
pub const HOLD_FOR_MARK: Duration = Duration::from_secs(2);

/// How often a held order looks for its first quote.
const MARK_POLL: Duration = Duration::from_millis(20);

fn reject(code: &str, message: String) -> RiskRejection {
    RiskRejection { code: code.to_string(), message }
}

/// The market data gateway owner name for a strategy's interest.
pub fn md_owner(strategy_code: &str) -> String {
    format!("strategy:{strategy_code}")
}

/// Only the strategy's own principal trades its portfolio.
pub fn may_trade(strategy: &Strategy, principal_id: Uuid) -> Result<(), RiskRejection> {
    if principal_id == strategy.principal_id {
        Ok(())
    } else {
        Err(reject("strategy_portfolio", format!("portfolio is traded by strategy {}", strategy.code)))
    }
}

pub fn check_state(strategy: &Strategy, side: OrderSide, position_qty: f64) -> Result<(), RiskRejection> {
    check_trading_state(strategy.state, side, position_qty)
}

/// The limits for one instrument: its `risk_limits` row whole, else the
/// strategy's defaults — whose notional limits are only read in a USD-like quote.
pub fn effective_limits(strategy: &Strategy, override_row: Option<RiskLimits>, quote_currency: &str) -> Result<RiskLimits, RiskRejection> {
    if let Some(limits) = override_row {
        return Ok(limits);
    }
    let d = strategy.limits;
    let has_notional = d.max_order_notional.is_some() || d.max_position_notional.is_some();
    if has_notional && !USD_LIKE.contains(&quote_currency) {
        return Err(reject("no_notional_limit", format!("no notional limit for quote currency {quote_currency}")));
    }
    Ok(d)
}

/// A limit price more than `band_pct` percent from `mid` is rejected.
pub fn check_price_band(limit_price: Option<f64>, mid: f64, band_pct: f64) -> Result<(), RiskRejection> {
    let Some(px) = limit_price else { return Ok(()) };
    let off = (px - mid).abs() / mid * 100.0;
    if off > band_pct {
        return Err(reject(
            "price_band",
            format!("limit {px} is {off:.2}% from mark {mid} (band {band_pct}%)"),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub enum MarkLookup {
    Live(Mark),
    /// No feed prices this instrument at all.
    NotCovered,
    TimedOut,
}

/// The live mark for `inst`, subscribing it for `owner` and waiting up to
/// `timeout` for a first quote when there is none yet.
pub async fn reference_mark(marks: &MarkStore, md: &MdHandle, owner: &str, inst: &InstrumentRef, covered: bool, timeout: Duration) -> MarkLookup {
    if !covered {
        return MarkLookup::NotCovered;
    }
    md.subscribe(owner, vec![inst.clone()]);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(mark) = marks.get(inst.id) {
            return MarkLookup::Live(mark);
        }
        if tokio::time::Instant::now() >= deadline {
            return MarkLookup::TimedOut;
        }
        tokio::time::sleep(MARK_POLL).await;
    }
}

/// Whether any feed this build ships prices `inst`.
pub fn covered(inst: &InstrumentRef) -> bool {
    crate::feeds::all().iter().any(|f| dataprovider::covers(*f, inst))
}

/// The mark a strategy order is checked against, looked up before its
/// transaction: `Ok(None)` when no feed covers the instrument.
pub async fn reference(state: &crate::app_state::AppState, strategy: &Strategy, inst: &InstrumentRef) -> Result<Option<Mark>, RiskRejection> {
    let owner = md_owner(&strategy.code);
    match reference_mark(state.marks(), &state.market_data().handle(), &owner, inst, covered(inst), HOLD_FOR_MARK).await {
        MarkLookup::Live(m) => Ok(Some(m)),
        MarkLookup::NotCovered => {
            warn!(strategy = %strategy.code, instrument_id = inst.id, "no feed prices this instrument; price band and market-order notional skipped");
            Ok(None)
        }
        MarkLookup::TimedOut => Err(reject("no_reference_price", format!("no reference price within {}s", HOLD_FOR_MARK.as_secs()))),
    }
}

/// Every strategy check for one order, inside its transaction. `strategy` is
/// the row as locked by `strategies::by_portfolio`.
pub async fn check(
    tx: &mut PgConnection,
    strategy: &Strategy,
    cmd: &SubmitOrder,
    quote_currency: &str,
    mark: Option<Mark>,
    contract_size: f64,
) -> Result<(), crate::risk_engine::RiskCheckError> {
    use crate::risk_engine::RiskCheckError;
    let scope = RiskScope { portfolio_id: strategy.portfolio_id, instrument_id: &cmd.instrument_id };
    let mut provider = PgRiskDataProvider::new(tx);
    let row = provider.risk_config(&scope).await?;
    let exposure = provider.exposure(&scope).await?;

    check_state(strategy, cmd.side, exposure.position_qty).map_err(RiskCheckError::Rejected)?;
    if let Some(row) = &row {
        check_trading_state(row.trading_state, cmd.side, exposure.position_qty).map_err(RiskCheckError::Rejected)?;
    }
    let limits = effective_limits(strategy, row.map(|r| r.limits), quote_currency).map_err(RiskCheckError::Rejected)?;

    let est_price = match mark {
        Some(m) => {
            if let Some(band) = strategy.price_band_pct {
                check_price_band(cmd.limit_price, m.mid(), band).map_err(RiskCheckError::Rejected)?;
            }
            Some(cmd.limit_price.unwrap_or(m.mid()) * contract_size)
        }
        None => cmd.limit_price.map(|p| p * contract_size),
    };
    check_limit(cmd.side, cmd.quantity, est_price, limits, exposure).map_err(RiskCheckError::Rejected)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::orders::risk::TradingState;
    use crate::marks::MarkStore;
    use crate::strategies::tests::sample;

    fn limits(max_order_notional: Option<f64>) -> RiskLimits {
        RiskLimits { max_order_quantity: None, max_order_notional, max_position_quantity: None, max_position_notional: None }
    }

    #[test]
    fn a_halted_strategy_admits_nothing() {
        let mut s = sample("s", "S", "OMS");
        s.state = TradingState::Halted;
        let e = check_state(&s, OrderSide::Sell, 5.0).unwrap_err();
        assert_eq!(e.code, "trading_halted");
    }

    #[test]
    fn the_cockpit_cannot_trade_a_strategy_portfolio() {
        let mut s = sample("s", "S", "OMS");
        s.principal_id = Uuid::new_v4();
        let e = may_trade(&s, Uuid::new_v4()).unwrap_err();
        assert_eq!(e.code, "strategy_portfolio");
        assert!(e.message.contains("strategy s"));
        assert!(may_trade(&s, s.principal_id).is_ok());
    }

    #[test]
    fn defaults_do_not_price_a_non_usd_quote() {
        let mut s = sample("s", "S", "OMS");
        s.limits = limits(Some(10_000.0));
        let e = effective_limits(&s, None, "BTC").unwrap_err();
        assert_eq!(e.message, "no notional limit for quote currency BTC");
        assert_eq!(effective_limits(&s, Some(limits(Some(5.0))), "BTC").unwrap().max_order_notional, Some(5.0));
        assert_eq!(effective_limits(&s, None, "USDT").unwrap().max_order_notional, Some(10_000.0));
    }

    #[test]
    fn the_band_rejects_a_price_too_far_from_mid() {
        assert!(check_price_band(Some(104.0), 100.0, 5.0).is_ok());
        let e = check_price_band(Some(106.0), 100.0, 5.0).unwrap_err();
        assert_eq!(e.code, "price_band");
        assert!(check_price_band(None, 100.0, 5.0).is_ok());
    }

    fn held(id: i64) -> dataprovider::InstrumentRef {
        dataprovider::InstrumentRef { id, symbol: "BTCUSDT".into(), instrument_class: None, asset_class: None, venue: Some("BINANCE".into()) }
    }

    fn md() -> gateway::md::MdHandle {
        gateway::md::MarketDataGateway::new(gateway::stream_health::StreamHealthRegistry::new()).0.handle()
    }

    #[tokio::test]
    async fn an_order_waits_for_the_first_quote() {
        let marks = MarkStore::new();
        let later = marks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            later.set(7, 99.0, 101.0);
        });
        match reference_mark(&marks, &md(), "strategy:s", &held(7), true, Duration::from_secs(2)).await {
            MarkLookup::Live(m) => assert_eq!(m.mid(), 100.0),
            other => panic!("expected a live mark, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_quote_within_the_timeout_is_timed_out() {
        let lookup = reference_mark(&MarkStore::new(), &md(), "strategy:s", &held(7), true, Duration::from_millis(50)).await;
        assert!(matches!(lookup, MarkLookup::TimedOut));
    }

    #[tokio::test]
    async fn an_uncovered_instrument_is_not_waited_for() {
        let lookup = reference_mark(&MarkStore::new(), &md(), "strategy:s", &held(7), false, Duration::from_secs(5)).await;
        assert!(matches!(lookup, MarkLookup::NotCovered));
    }
}
