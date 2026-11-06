/*
Invariants protect the consistency of an aggregate order.
The process is:
command -> check domain invariant -> valid? or not? -> emit event or reject command
*/

use super::errors::{CommandRejection, RejectionCode};
use super::state::{OrderAggregateState, OrderType};

pub fn validate_submit(
    quantity: f64,
    order_type: OrderType,
    limit_price: Option<f64>,
) -> Result<(), CommandRejection> {
    if !(quantity > 0.0 && quantity.is_finite()) {
        return Err(CommandRejection::new(
            RejectionCode::InvalidQuantity,
            "order quantity must be greater than zero",
        ));
    }

    if matches!(order_type, OrderType::Limit) && !limit_price.is_some_and(|p| p > 0.0 && p.is_finite()) {
        return Err(CommandRejection::new(
            RejectionCode::InvalidPrice,
            "limit orders require a positive limit_price",
        ));
    }

    Ok(())
}

pub fn require_existing_order(
    state: &Option<OrderAggregateState>,
) -> Result<&OrderAggregateState, CommandRejection> {
    state.as_ref().ok_or_else(|| {
        CommandRejection::new(RejectionCode::OrderNotFound, "order stream does not exist")
    })
}

#[cfg(test)]
mod finite_tests {
    use super::*;

    #[test]
    fn a_non_finite_quantity_or_price_is_rejected() {
        assert!(validate_submit(f64::NAN, OrderType::Market, None).is_err());
        assert!(validate_submit(f64::INFINITY, OrderType::Market, None).is_err());
        assert!(validate_submit(1.0, OrderType::Limit, Some(f64::NAN)).is_err());
        assert!(validate_submit(1.0, OrderType::Limit, Some(f64::INFINITY)).is_err());
        assert!(validate_submit(1.0, OrderType::Limit, Some(2.0)).is_ok());
    }
}
