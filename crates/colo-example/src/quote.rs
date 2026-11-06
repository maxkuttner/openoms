//! Where the market maker quotes, and how it names its orders.

/// One bid and one ask `half_spread_bps` either side of `mid`, rounded away
/// from the mid onto `tick`, so the bid is always below and the ask above.
pub fn quotes(mid: f64, half_spread_bps: f64, tick: f64) -> (f64, f64) {
    let half = mid * half_spread_bps / 10_000.0;
    let bid = ((mid - half) / tick).floor() * tick;
    let ask = ((mid + half) / tick).ceil() * tick;
    (bid, ask)
}

/// A client order id carrying the strategy's tag (`mm1-q-000007`), which the
/// OMS shows on the fill.
pub fn client_id(tag: &str, n: u64) -> String {
    format!("{tag}-{n:06}")
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
    fn client_ids_carry_the_tag() {
        assert_eq!(client_id("mm1-q", 7), "mm1-q-000007");
    }
}
