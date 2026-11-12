//! Venue fee domain: how a reported fee affects the position (pure part).

use serde::{Deserialize, Serialize};

/// How a fee in a given currency hits the position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeeEffect {
    /// Fee in the instrument currency: reduces realized P&L.
    Quote,
    /// Fee in the base asset of a pair: shrinks the held quantity.
    Base,
    /// Any other currency: tracked separately, no position effect.
    Other,
}

/// Classify a fee by its currency relative to the instrument (case-insensitive).
pub fn fee_effect(
    fee_currency: &str,
    symbol: &str,
    asset_class: &str,
    instrument_currency: &str,
) -> FeeEffect {
    let fee = fee_currency.to_uppercase();
    let quote = instrument_currency.to_uppercase();
    if fee == quote {
        return FeeEffect::Quote;
    }
    let class = asset_class.to_uppercase();
    if class == "CRYPTO" || class == "FX" {
        let sym: String = symbol
            .chars()
            .filter(|c| !matches!(c, '-' | '.' | '/' | '_'))
            .collect::<String>()
            .to_uppercase();
        if let Some(rest) = sym.strip_suffix(&quote) {
            if rest == fee {
                return FeeEffect::Base;
            }
        }
    }
    FeeEffect::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn usdt_fee_on_btcusdt_is_quote() { assert_eq!(fee_effect("USDT", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Quote); }
    #[test] fn btc_fee_on_btcusdt_is_base() { assert_eq!(fee_effect("btc", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Base); }
    #[test] fn base_fee_with_separator_symbol() {
        assert_eq!(fee_effect("BTC", "BTC-USD", "CRYPTO", "USD"), FeeEffect::Base);
        assert_eq!(fee_effect("EUR", "EUR.USD", "FX", "USD"), FeeEffect::Base);
    }
    #[test] fn bnb_fee_on_btcusdt_is_other() { assert_eq!(fee_effect("BNB", "BTCUSDT", "CRYPTO", "USDT"), FeeEffect::Other); }
    #[test] fn usd_commission_on_cad_stock_is_other() { assert_eq!(fee_effect("USD", "SHOP", "EQUITY", "CAD"), FeeEffect::Other); }
    #[test] fn equity_never_has_a_base() { assert_eq!(fee_effect("AAPL", "AAPL", "EQUITY", "USD"), FeeEffect::Other); }
    #[test] fn serde_names_are_lowercase() {
        assert_eq!(serde_json::to_string(&FeeEffect::Quote).unwrap(), "\"quote\"");
        assert_eq!(serde_json::from_str::<FeeEffect>("\"base\"").unwrap(), FeeEffect::Base);
    }
}
