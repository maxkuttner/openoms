use super::state::OrderSide;

// A strategy/portfolio/trader is in a particular trading state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingState { Active, Reducing, Halted}


// TODO: refine which risk limit shall be part of the oms risk check
#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
}


#[derive(Debug, Clone, Copy)]
pub struct Exposure {
    pub position_qty: f64, // filled position qty
    pub working_qty: f64,  // still open position qty
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskRejection { pub code: String, pub message: String}


pub fn check_trading_state(
    state: TradingState,
    side: OrderSide,
    position_qty: f64,
) -> Result<(), RiskRejection>{
    match state {
        TradingState::Active => Ok(()),
        TradingState::Halted => Err(
            RiskRejection {
                code: "trading_halted".into(),
                message: "trading halted".to_string(),
        }),
        TradingState::Reducing => {
            // Only orders which shrink abs(position) are allowed
            let reduces = match side {
                OrderSide::Buy => position_qty < 0.0,
                OrderSide::Sell => position_qty > 0.0,
            };
            if reduces {
                Ok(())
            } else {
                Err(RiskRejection {
                    code: "trading_reducing".to_string(),
                    message: format!("trading state reducing; order {} does not reduce the current position", side.as_str()).to_string(),
                })
            }
        }
    }
}


pub fn check_limit(
    side: OrderSide,
    quantity: f64,
    est_price: Option<f64>,
    limits: RiskLimits,
    exposure: Exposure,
) -> Result<(), RiskRejection> {

    // 1) check order quantity
    if let Some(max) = limits.max_order_quantity {
        if quantity > max {
            return Err(RiskRejection {
                code: "max_order_qty".into(),
                message: format!("order qty {quantity} exceeds max {max}"),
            });
        }
    }

    // 2) check order notional
    if let (Some(max), Some(px)) = (limits.max_order_notional, est_price) {
        let notional = quantity * px;
        if notional > max {
            return Err(RiskRejection {
                code: "max_order_notional".into(),
                message: format!("order notional exceeds {max} < {notional}")
            });
        }
    }

    // 3) check max instrument position
    let signed = match side { OrderSide::Buy => quantity, OrderSide::Sell => -quantity};
    let projected_qty = exposure.position_qty + exposure.working_qty + signed;
    if let Some(max) = limits.max_position_quantity {
        if projected_qty.abs() > max {
            return Err(RiskRejection {
                code: "max_position_quantity".into(),
                message: format!("order qty exceeds max position quantity {max}")
            })
        }
    }

    // 4) check resulting positional notional
    if let (Some(max), Some(px)) = (limits.max_position_notional, est_price){
        let notional = projected_qty.abs() * px;
        if notional > max {
            return Err(RiskRejection {
                code: "max_position_notional".into(),
                message: format!("projected positional notional {notional} exceeds max {max}")
            });
        }
    }

    // else -> ()
    Ok(())
}


#[cfg(test)]
mod tests {
    use crate::domain::orders::risk::{
        check_limit, check_trading_state, Exposure, RiskLimits, RiskRejection, TradingState,
    };
    use crate::domain::orders::state::OrderSide;

    // --- helpers -----------------------------------------------------------
    // Fresh structs per call, because `check_limit` takes them by value.
    fn limits(
        max_order_quantity: Option<f64>,
        max_order_notional: Option<f64>,
        max_position_quantity: Option<f64>,
        max_position_notional: Option<f64>,
    ) -> RiskLimits {
        RiskLimits {
            max_order_quantity,
            max_order_notional,
            max_position_quantity,
            max_position_notional,
        }
    }

    fn exposure(position_qty: f64, working_qty: f64) -> Exposure {
        Exposure { position_qty, working_qty }
    }

    fn assert_rejected(result: Result<(), RiskRejection>, expected_code: &str) {
        let err = result.expect_err("expected a rejection");
        assert_eq!(err.code, expected_code);
    }

    #[test]
    fn check_trading_state_halted(){
        let state = TradingState::Halted;
        let err = check_trading_state(state, OrderSide::Buy, 123.123).unwrap_err();
        assert_eq!(err.code, "trading_halted");
    }

    #[test]
    fn check_trading_state_active(){
        let state = TradingState::Active;
        assert!(check_trading_state(state, OrderSide::Buy, 123.123).is_ok());
    }

    #[test]
    fn check_trading_state_reducing_err(){
        let state = TradingState::Reducing;
        let err = check_trading_state(state, OrderSide::Buy, 123.123).unwrap_err();
        assert_eq!(err.code, "trading_reducing");
    }


    #[test]
    fn check_trading_state_reducing_ok(){
        let state = TradingState::Reducing;
        assert!(check_trading_state(state, OrderSide::Buy, -123.123).is_ok());
    }

    // --- check_limit: no limits configured ---------------------------------
    #[test]
    fn no_limits_configured_ok() {
        let r = check_limit(
            OrderSide::Buy, 999.0, Some(999.0),
            limits(None, None, None, None),
            exposure(0.0, 0.0),
        );
        assert!(r.is_ok());
    }

    // --- check_limit: max_order_quantity -----------------------------------
    #[test]
    fn order_qty_within_limit_ok() {
        let r = check_limit(
            OrderSide::Buy, 50.0, None,
            limits(Some(100.0), None, None, None),
            exposure(0.0, 0.0),
        );
        assert!(r.is_ok());
    }

    #[test]
    fn order_qty_exceeds_limit_rejected() {
        let r = check_limit(
            OrderSide::Buy, 150.0, None,
            limits(Some(100.0), None, None, None),
            exposure(0.0, 0.0),
        );
        assert_rejected(r, "max_order_qty");
    }

    // --- check_limit: max_order_notional -----------------------------------
    #[test]
    fn order_notional_exceeds_limit_rejected() {
        // 50 * 10 = 500 > 400
        let r = check_limit(
            OrderSide::Buy, 50.0, Some(10.0),
            limits(None, Some(400.0), None, None),
            exposure(0.0, 0.0),
        );
        assert_rejected(r, "max_order_notional");
    }

    #[test]
    fn order_notional_skipped_without_price() {
        // No est_price -> notional check is a no-op even with a tiny limit.
        // Documents the market-order gap *intentionally*.
        let r = check_limit(
            OrderSide::Buy, 50.0, None,
            limits(None, Some(1.0), None, None),
            exposure(0.0, 0.0),
        );
        assert!(r.is_ok());
    }

    // --- check_limit: max_position_quantity --------------------------------
    #[test]
    fn projected_position_exceeds_limit_rejected() {
        // position 80 + working 0 + buy 50 = 130 > 100
        let r = check_limit(
            OrderSide::Buy, 50.0, None,
            limits(None, None, Some(100.0), None),
            exposure(80.0, 0.0),
        );
        assert_rejected(r, "max_position_quantity");
    }

    #[test]
    fn working_orders_count_toward_position_limit() {
        // order itself is tiny (10), but position 60 + working 60 + 10 = 130 > 100
        let r = check_limit(
            OrderSide::Buy, 10.0, None,
            limits(None, None, Some(100.0), None),
            exposure(60.0, 60.0),
        );
        assert_rejected(r, "max_position_quantity");
    }

    #[test]
    fn large_sell_that_reduces_long_position_ok() {
        // long 90, sell 80 -> projected 10; checks the *resulting* position,
        // not the order size, so this passes a position cap of 100.
        let r = check_limit(
            OrderSide::Sell, 80.0, None,
            limits(None, None, Some(100.0), None),
            exposure(90.0, 0.0),
        );
        assert!(r.is_ok());
    }

    // --- check_limit: max_position_notional --------------------------------
    #[test]
    fn projected_position_notional_exceeds_limit_rejected() {
        // position 80 + buy 50 = 130; 130 * 10 = 1300 > 1000.
        // Regression guard for the bug where this used order qty (50*10=500).
        let r = check_limit(
            OrderSide::Buy, 50.0, Some(10.0),
            limits(None, None, None, Some(1000.0)),
            exposure(80.0, 0.0),
        );
        assert_rejected(r, "max_position_notional");
    }
}

