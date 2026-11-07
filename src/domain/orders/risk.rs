use crate::domain::orders::state::OrderSide;

pub use risk::{Exposure, Limits as RiskLimits, TradingState};

/// The existing HTTP/FIX rejection shape, kept as an adapter over [`risk`].
pub type RiskRejection = risk::Reject;

fn shared_side(side: OrderSide) -> risk::Side {
    match side {
        OrderSide::Buy => risk::Side::Buy,
        OrderSide::Sell => risk::Side::Sell,
    }
}

fn adapt(rejection: risk::Reject) -> RiskRejection {
    // The OMS/FIX surface has used the long spelling since before the shared
    // evaluator; keep that wire shape.
    let code = if rejection.code == "max_position_qty" {
        "max_position_quantity".to_string()
    } else {
        rejection.code
    };
    RiskRejection { code, message: rejection.message }
}

/// Run the shared evaluator, mapping its rejection codes to the OMS surface.
pub fn evaluate(input: &risk::RiskInput) -> Result<(), RiskRejection> {
    risk::evaluate(input).map_err(adapt)
}

/// Trading-state gate only: no limits, no valuation. `quantity` and the full
/// directional exposure are required so `Reducing` can reserve working orders.
pub fn check_trading_state(
    state: TradingState,
    side: OrderSide,
    quantity: f64,
    exposure: &Exposure,
) -> Result<(), RiskRejection> {
    evaluate(&risk::RiskInput {
        order: risk::OrderInput {
            side: shared_side(side),
            quantity,
            limit_price: None,
            multiplier: 1.0,
        },
        exposure: *exposure,
        limits: RiskLimits::default(),
        state,
        valuation: risk::Valuation::default(),
    })
}

/// Numeric limits only. `limit_price` values the candidate when present;
/// `price` values a market order and the projected position when supplied.
/// `multiplier` is applied exactly once.
pub fn check_limit(
    side: OrderSide,
    quantity: f64,
    limit_price: Option<f64>,
    price: Option<f64>,
    multiplier: f64,
    limits: RiskLimits,
    exposure: &Exposure,
) -> Result<(), RiskRejection> {
    evaluate(&risk::RiskInput {
        order: risk::OrderInput {
            side: shared_side(side),
            quantity,
            limit_price,
            multiplier,
        },
        exposure: *exposure,
        limits,
        state: TradingState::Active,
        valuation: risk::Valuation {
            order_price: price,
            position_price: price,
            reference_price: None,
        },
    })
}

/// Price-band only. `None` limit price passes (market orders have no price to
/// band-check).
pub fn check_price_band(
    limit_price: Option<f64>,
    reference: f64,
    band_pct: f64,
) -> Result<(), RiskRejection> {
    evaluate(&risk::RiskInput {
        order: risk::OrderInput {
            side: shared_side(OrderSide::Buy),
            quantity: 1.0,
            limit_price,
            multiplier: 1.0,
        },
        exposure: Exposure::default(),
        limits: RiskLimits { price_band_pct: Some(band_pct), ..RiskLimits::default() },
        state: TradingState::Active,
        valuation: risk::Valuation {
            order_price: None,
            position_price: None,
            reference_price: Some(reference),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(
        max_order_quantity: Option<f64>,
        max_order_notional: Option<f64>,
        max_position_quantity: Option<f64>,
        max_position_notional: Option<f64>,
    ) -> RiskLimits {
        RiskLimits {
            max_order_qty: max_order_quantity,
            max_order_notional,
            max_position_qty: max_position_quantity,
            max_position_notional,
            ..RiskLimits::default()
        }
    }

    fn exposure(position_qty: f64, working_buy_qty: f64, working_sell_qty: f64) -> Exposure {
        Exposure { position_qty, working_buy_qty, working_sell_qty }
    }

    fn assert_rejected(result: Result<(), RiskRejection>, expected_code: &str) {
        let err = result.expect_err("expected a rejection");
        assert_eq!(err.code, expected_code);
    }

    #[test]
    fn check_trading_state_halted() {
        let err = check_trading_state(
            TradingState::Halted,
            OrderSide::Buy,
            1.0,
            &Exposure::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, "trading_halted");
    }

    #[test]
    fn check_trading_state_active() {
        assert!(check_trading_state(
            TradingState::Active,
            OrderSide::Buy,
            1.0,
            &Exposure::default()
        )
        .is_ok());
    }

    #[test]
    fn check_trading_state_reducing_err() {
        let err = check_trading_state(
            TradingState::Reducing,
            OrderSide::Buy,
            1.0,
            &exposure(123.123, 0.0, 0.0),
        )
        .unwrap_err();
        assert_eq!(err.code, "trading_reducing");
    }

    #[test]
    fn check_trading_state_reducing_ok() {
        assert!(check_trading_state(
            TradingState::Reducing,
            OrderSide::Buy,
            1.0,
            &exposure(-123.123, 0.0, 0.0)
        )
        .is_ok());
    }

    #[test]
    fn check_trading_state_reducing_reserves_working_sells() {
        let err = check_trading_state(
            TradingState::Reducing,
            OrderSide::Sell,
            5.0,
            &exposure(10.0, 0.0, 6.0),
        )
        .unwrap_err();
        assert_eq!(err.code, "trading_reducing");
    }

    #[test]
    fn no_limits_configured_ok() {
        assert!(check_limit(
            OrderSide::Buy,
            999.0,
            Some(999.0),
            Some(999.0),
            1.0,
            limits(None, None, None, None),
            &exposure(0.0, 0.0, 0.0),
        )
        .is_ok());
    }

    #[test]
    fn order_qty_within_limit_ok() {
        assert!(check_limit(
            OrderSide::Buy,
            50.0,
            None,
            None,
            1.0,
            limits(Some(100.0), None, None, None),
            &exposure(0.0, 0.0, 0.0),
        )
        .is_ok());
    }

    #[test]
    fn order_qty_exceeds_limit_rejected() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                150.0,
                None,
                None,
                1.0,
                limits(Some(100.0), None, None, None),
                &exposure(0.0, 0.0, 0.0),
            ),
            "max_order_qty",
        );
    }

    #[test]
    fn order_notional_exceeds_limit_rejected() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                50.0,
                Some(10.0),
                Some(10.0),
                1.0,
                limits(None, Some(400.0), None, None),
                &exposure(0.0, 0.0, 0.0),
            ),
            "max_order_notional",
        );
    }

    #[test]
    fn order_notional_skipped_without_price() {
        assert!(check_limit(
            OrderSide::Buy,
            50.0,
            None,
            None,
            1.0,
            limits(None, Some(1.0), None, None),
            &exposure(0.0, 0.0, 0.0),
        )
        .is_ok());
    }

    #[test]
    fn projected_position_exceeds_limit_rejected() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                50.0,
                None,
                None,
                1.0,
                limits(None, None, Some(100.0), None),
                &exposure(80.0, 0.0, 0.0),
            ),
            "max_position_quantity",
        );
    }

    #[test]
    fn working_orders_count_toward_position_limit() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                10.0,
                None,
                None,
                1.0,
                limits(None, None, Some(100.0), None),
                &exposure(60.0, 60.0, 0.0),
            ),
            "max_position_quantity",
        );
    }

    #[test]
    fn opposing_working_orders_do_not_offset() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                100.0,
                None,
                None,
                1.0,
                limits(None, None, Some(100.0), None),
                &exposure(0.0, 100.0, 100.0),
            ),
            "max_position_quantity",
        );
    }

    #[test]
    fn large_sell_that_reduces_long_position_ok() {
        assert!(check_limit(
            OrderSide::Sell,
            80.0,
            None,
            None,
            1.0,
            limits(None, None, Some(100.0), None),
            &exposure(90.0, 0.0, 0.0),
        )
        .is_ok());
    }

    #[test]
    fn projected_position_notional_exceeds_limit_rejected() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                50.0,
                Some(10.0),
                Some(10.0),
                1.0,
                limits(None, None, None, Some(1000.0)),
                &exposure(80.0, 0.0, 0.0),
            ),
            "max_position_notional",
        );
    }

    #[test]
    fn contract_multiplier_is_applied_once() {
        assert_rejected(
            check_limit(
                OrderSide::Buy,
                2.0,
                Some(50.0),
                None,
                100.0,
                limits(None, Some(9_999.0), None, None),
                &Exposure::default(),
            ),
            "max_order_notional",
        );
        assert!(check_limit(
            OrderSide::Buy,
            2.0,
            Some(50.0),
            None,
            100.0,
            limits(None, Some(10_000.0), None, None),
            &Exposure::default(),
        )
        .is_ok());
    }

    #[test]
    fn price_band_rejects_far_limit() {
        assert!(check_price_band(Some(104.0), 100.0, 5.0).is_ok());
        assert_rejected(check_price_band(Some(106.0), 100.0, 5.0), "price_band");
        assert!(check_price_band(None, 100.0, 5.0).is_ok());
    }
}
