//! `oms setup seed-test-chain` — synthetic SPY option chain for exercising
//! instrument search and the option chain UI without a live broker sync.
//!
//! An SPY equity row plus a strike-ladder of OPTION `InstrumentDef`s across the
//! next few Fridays, fed through the same [`catalog::upsert_catalog`] a real
//! broker sync uses — so this data is indistinguishable, downstream, from
//! anything `sync-broker` would have written.

use chrono::{Datelike, Duration, NaiveDate, Utc, Weekday};
use clap::Args as ClapArgs;
use dataprovider::{DerivativeDef, Identifiers, InstrumentDef, OptionKind};
use sqlx::PgPool;
use tracing::info;

use crate::setup::catalog;

const UNDERLYING: &str = "SPY";
const VENUE_EQUITY: &str = "ARCX";
const VENUE_OPTION: &str = "OPRA";
const REFERENCE_SPOT: f64 = 575.0;
const STRIKE_STEP: f64 = 5.0;
const STRIKES_EACH_SIDE: i64 = 10;
const EXPIRY_COUNT: usize = 5;

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {}

pub async fn run(_args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&super::database_url()?).await?;
    let defs = spy_chain_defs(Utc::now().date_naive());
    // No enrichers: this data has no vendor identifiers to enrich, and it must
    // never make a network call.
    let (summary, _ids) = catalog::upsert_catalog(&pool, &defs, &[]).await?;
    info!(
        "seed-test-chain done: upserted={} skipped_fk={} derivatives={} dated={}",
        summary.upserted,
        summary.skipped_fk(),
        summary.derivatives,
        summary.dated
    );
    Ok(())
}

/// The next `count` Fridays strictly after `today` (standard equity options
/// expire Friday). Deterministic from `today`, so a chain seeded this week and
/// again next week both land on real, unexpired dates — `upsert_catalog`
/// drops anything already expired, so there would be no point doing otherwise.
fn next_fridays(today: NaiveDate, count: usize) -> Vec<NaiveDate> {
    let mut d = today;
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        d += Duration::days(1);
        if d.weekday() == Weekday::Fri {
            out.push(d);
        }
    }
    out
}

/// OCC-style option symbol: ROOT + YYMMDD + C/P + strike*1000 zero-padded to 8
/// digits — the same convention `SubmitOrderRequest`'s docs and the Alpaca
/// adapter's live contracts both already use (e.g. `SPY260918C00770000`).
fn occ_symbol(underlying: &str, expiry: NaiveDate, kind: OptionKind, strike: f64) -> String {
    let cp = match kind {
        OptionKind::Call => 'C',
        OptionKind::Put => 'P',
    };
    format!(
        "{underlying}{:02}{:02}{:02}{cp}{:08}",
        expiry.year() % 100,
        expiry.month(),
        expiry.day(),
        (strike * 1000.0).round() as i64,
    )
}

/// Synthetic SPY equity + option chain: 1 underlying plus `EXPIRY_COUNT`
/// expiries x (2 * `STRIKES_EACH_SIDE` + 1) strikes x call/put.
pub fn spy_chain_defs(today: NaiveDate) -> Vec<InstrumentDef> {
    let mut defs = Vec::new();

    defs.push(InstrumentDef {
        symbol: UNDERLYING.to_string(),
        venue: VENUE_EQUITY.to_string(),
        currency: "USD".to_string(),
        asset_class: "EQUITY".to_string(),
        instrument_class: "SPOT".to_string(),
        name: Some("SPDR S&P 500 ETF Trust".to_string()),
        price_precision: 2,
        price_increment: 0.01,
        size_increment: 1.0,
        lot_size: None,
        contract_size: 1.0,
        native_id: None,
        provider_exchange: None,
        derivative: None,
        identifiers: Identifiers::default(),
    });

    for expiry in next_fridays(today, EXPIRY_COUNT) {
        for i in -STRIKES_EACH_SIDE..=STRIKES_EACH_SIDE {
            let strike = REFERENCE_SPOT + (i as f64) * STRIKE_STEP;
            for kind in [OptionKind::Call, OptionKind::Put] {
                defs.push(InstrumentDef {
                    symbol: occ_symbol(UNDERLYING, expiry, kind, strike),
                    venue: VENUE_OPTION.to_string(),
                    currency: "USD".to_string(),
                    asset_class: "EQUITY".to_string(),
                    instrument_class: "OPTION".to_string(),
                    name: None,
                    price_precision: 2,
                    price_increment: 0.01,
                    size_increment: 1.0,
                    lot_size: None,
                    contract_size: 100.0,
                    native_id: None,
                    provider_exchange: Some(VENUE_OPTION.to_string()),
                    derivative: Some(DerivativeDef {
                        underlying_symbol: UNDERLYING.to_string(),
                        option_kind: Some(kind),
                        strike_price: Some(strike),
                        expiry_date: Some(expiry),
                        activation_date: None,
                    }),
                    identifiers: Identifiers::default(),
                });
            }
        }
    }

    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn next_fridays_returns_the_requested_count_of_strictly_future_fridays() {
        // 2026-09-28 is a Monday.
        let fridays = next_fridays(date(2026, 9, 28), 5);
        assert_eq!(fridays.len(), 5);
        assert_eq!(fridays[0], date(2026, 10, 2));
        for f in &fridays {
            assert_eq!(f.weekday(), Weekday::Fri);
            assert!(*f > date(2026, 9, 28));
        }
    }

    #[test]
    fn next_fridays_from_a_friday_excludes_that_day() {
        let fridays = next_fridays(date(2026, 10, 2), 1);
        assert_eq!(fridays, vec![date(2026, 10, 9)]);
    }

    #[test]
    fn occ_symbol_matches_the_documented_convention() {
        let call = occ_symbol("SPY", date(2026, 10, 17), OptionKind::Call, 575.0);
        assert_eq!(call, "SPY261017C00575000");
        let put = occ_symbol("SPY", date(2026, 10, 17), OptionKind::Put, 572.5);
        assert_eq!(put, "SPY261017P00572500");
    }

    #[test]
    fn spy_chain_defs_seeds_the_underlying_plus_a_full_ladder() {
        let defs = spy_chain_defs(date(2026, 9, 28));

        let underlyings: Vec<_> = defs.iter().filter(|d| d.derivative.is_none()).collect();
        assert_eq!(underlyings.len(), 1);
        assert_eq!(underlyings[0].symbol, "SPY");
        assert_eq!(underlyings[0].instrument_class, "SPOT");

        let options: Vec<_> = defs.iter().filter(|d| d.derivative.is_some()).collect();
        // 5 expiries x 21 strikes x 2 (call/put)
        assert_eq!(options.len(), 5 * 21 * 2);
        for d in &options {
            assert_eq!(d.instrument_class, "OPTION");
            assert_eq!(d.venue, VENUE_OPTION);
            assert_eq!(d.contract_size, 100.0);
            let dv = d.derivative.as_ref().unwrap();
            assert_eq!(dv.underlying_symbol, "SPY");
            assert!(dv.expiry_date.unwrap() > date(2026, 9, 28));
        }

        let mut symbols: Vec<&str> = defs.iter().map(|d| d.symbol.as_str()).collect();
        let before = symbols.len();
        symbols.sort();
        symbols.dedup();
        assert_eq!(symbols.len(), before, "no duplicate symbols");
    }
}
