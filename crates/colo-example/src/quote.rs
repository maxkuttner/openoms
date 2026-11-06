//! Where the market maker quotes, and how it names its orders.

/// One bid and one ask `half_spread_bps` either side of `mid`, rounded away
/// from the mid onto `tick`, so the bid is always below and the ask above.
pub fn quotes(mid: f64, half_spread_bps: f64, tick: f64) -> (f64, f64) {
    let half = mid * half_spread_bps / 10_000.0;
    // Count in ticks and divide by ticks-per-unit: `n * 0.01` leaves float
    // noise (76409.54000000001) that Binance rejects; `n / 100.0` does not.
    if tick >= 1.0 {
        return (((mid - half) / tick).floor() * tick, ((mid + half) / tick).ceil() * tick);
    }
    let per_unit = (1.0 / tick).round();
    let bid = ((mid - half) * per_unit).floor() / per_unit;
    let ask = ((mid + half) * per_unit).ceil() / per_unit;
    (bid, ask)
}

/// A client order id carrying the strategy's tag (`mm1-q-000007`), which the
/// OMS shows on the fill.
pub fn client_id(tag: &str, n: u64) -> String {
    format!("{tag}-{n:06}")
}

/// The public feed's symbol map: Binance reports pairs uppercase, and the
/// feed matches on what it reports.
pub fn feed_symbols(symbol: &str) -> std::collections::HashMap<String, i64> {
    std::collections::HashMap::from([(symbol.to_uppercase(), 1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_straddle_the_mid_on_the_tick() {
        let (bid, ask) = quotes(64_000.0, 20.0, 0.01);
        assert!(bid < 64_000.0 && 64_000.0 < ask);
        assert!(((bid / 0.01).round() * 0.01 - bid).abs() < 1e-9);
        assert!(((ask / 0.01).round() * 0.01 - ask).abs() < 1e-9);
        assert!((bid - 63_872.0).abs() < 0.011, "{bid}");
        assert!((ask - 64_128.0).abs() < 0.011, "{ask}");
    }

    #[test]
    fn the_feed_is_asked_for_the_pair_as_binance_reports_it() {
        let symbols = feed_symbols("btcusdt");
        assert_eq!(symbols.keys().collect::<Vec<_>>(), ["BTCUSDT"]);
    }

    #[test]
    fn quoted_prices_have_no_more_decimals_than_the_tick() {
        for i in 0..2000 {
            let mid = 76_257.0 + i as f64 * 0.0037;
            let (bid, ask) = quotes(mid, 20.0, 0.01);
            for p in [bid, ask] {
                let text = p.to_string();
                let decimals = text.split('.').nth(1).map_or(0, str::len);
                assert!(decimals <= 2, "{mid} quoted as {text}");
            }
        }
    }

    #[test]
    fn a_whole_number_tick_still_quotes_on_the_tick() {
        assert_eq!(quotes(1_003.0, 0.0, 5.0), (1_000.0, 1_005.0));
    }

    #[test]
    fn client_ids_carry_the_tag() {
        assert_eq!(client_id("mm1-q", 7), "mm1-q-000007");
    }
}
