use crate::{
    evaluate, resolve_limits, Exposure, Limits, OrderInput, Reject, RiskInput, Side, TradingState,
    Valuation,
};

fn code<T: std::fmt::Debug>(result: Result<T, Reject>) -> String {
    result.expect_err("expected a rejection").code
}

fn order(side: Side, quantity: f64) -> OrderInput {
    OrderInput { side, quantity, limit_price: Some(100.0), multiplier: 1.0 }
}

fn active_valuation() -> Valuation {
    Valuation {
        order_price: Some(100.0),
        position_price: Some(100.0),
        reference_price: Some(100.0),
    }
}

#[test]
fn opposing_working_orders_do_not_offset() {
    let input = RiskInput {
        order: OrderInput { side: Side::Buy, quantity: 100.0, limit_price: Some(1.0), multiplier: 1.0 },
        exposure: Exposure { position_qty: 0.0, working_buy_qty: 100.0, working_sell_qty: 100.0 },
        limits: Limits { max_position_qty: Some(100.0), ..Default::default() },
        state: TradingState::Active,
        valuation: active_valuation(),
    };

    assert_eq!(code(evaluate(&input)), "max_position_qty");
}

#[test]
fn reducing_cannot_cross_flat() {
    let input = RiskInput {
        order: order(Side::Sell, 11.0),
        exposure: Exposure { position_qty: 10.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
        limits: Limits::default(),
        state: TradingState::Reducing,
        valuation: active_valuation(),
    };

    assert_eq!(code(evaluate(&input)), "trading_reducing");
}

#[test]
fn reducing_reserves_remaining_capacity() {
    let input = RiskInput {
        order: order(Side::Sell, 5.0),
        exposure: Exposure { position_qty: 10.0, working_buy_qty: 0.0, working_sell_qty: 6.0 },
        limits: Limits::default(),
        state: TradingState::Reducing,
        valuation: active_valuation(),
    };

    assert_eq!(code(evaluate(&input)), "trading_reducing");
}

#[test]
fn multiplier_applies_once() {
    let mut input = RiskInput {
        order: OrderInput { side: Side::Buy, quantity: 2.0, limit_price: Some(50.0), multiplier: 100.0 },
        exposure: Exposure::default(),
        limits: Limits { max_order_notional: Some(9_999.0), ..Default::default() },
        state: TradingState::Active,
        valuation: Valuation::default(),
    };

    assert_eq!(code(evaluate(&input)), "max_order_notional");

    input.limits.max_order_notional = Some(10_000.0);
    assert!(evaluate(&input).is_ok());
}

#[test]
fn invalid_inputs_rejected() {
    let mut input = RiskInput {
        order: order(Side::Buy, 1.0),
        exposure: Exposure::default(),
        limits: Limits::default(),
        state: TradingState::Active,
        valuation: active_valuation(),
    };

    input.order.quantity = f64::NAN;
    assert_eq!(code(evaluate(&input)), "invalid_quantity");

    input.order.quantity = f64::INFINITY;
    assert_eq!(code(evaluate(&input)), "invalid_quantity");

    input.order.quantity = -1.0;
    assert_eq!(code(evaluate(&input)), "invalid_quantity");

    input.order.quantity = 1.0;
    input.limits.price_band_pct = Some(5.0);
    input.valuation.reference_price = Some(0.0);
    assert_eq!(code(evaluate(&input)), "invalid_reference_price");
}

#[test]
fn position_valuation_not_candidate_bid() {
    let input = RiskInput {
        order: OrderInput { side: Side::Sell, quantity: 10.0, limit_price: Some(1.0), multiplier: 1.0 },
        exposure: Exposure { position_qty: 10.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
        limits: Limits { max_position_notional: Some(500.0), ..Default::default() },
        state: TradingState::Active,
        valuation: Valuation {
            order_price: Some(1.0),
            position_price: Some(100.0),
            reference_price: Some(1.0),
        },
    };

    assert_eq!(code(evaluate(&input)), "max_position_notional");
}

#[test]
fn overrides_win_per_field() {
    let defaults = Limits {
        max_order_qty: Some(5.0),
        max_order_notional: Some(100.0),
        max_position_qty: Some(10.0),
        max_position_notional: Some(1_000.0),
        price_band_pct: Some(5.0),
        max_orders_per_sec: Some(3),
    };
    let overrides = Limits {
        max_order_qty: None,
        max_order_notional: Some(50.0),
        max_position_qty: Some(7.0),
        max_position_notional: None,
        price_band_pct: None,
        max_orders_per_sec: Some(1),
    };

    let resolved = resolve_limits(&defaults, &overrides, "USDT").expect("valid");

    assert_eq!(resolved.max_order_qty, Some(5.0));
    assert_eq!(resolved.max_order_notional, Some(50.0));
    assert_eq!(resolved.max_position_qty, Some(7.0));
    assert_eq!(resolved.max_position_notional, Some(1_000.0));
    assert_eq!(resolved.price_band_pct, Some(5.0));
    assert_eq!(resolved.max_orders_per_sec, Some(1));
}

#[test]
fn inherited_notional_needs_a_usd_like_quote() {
    let defaults = Limits { max_order_notional: Some(100.0), ..Default::default() };

    assert_eq!(
        code(resolve_limits(&defaults, &Limits::default(), "BTC")),
        "no_notional_limit"
    );
    assert!(resolve_limits(&defaults, &Limits::default(), "USDT").is_ok());
}

#[test]
fn explicit_notional_override_is_allowed_for_any_quote() {
    let defaults = Limits { max_order_notional: Some(100.0), ..Default::default() };
    let overrides = Limits { max_order_notional: Some(50.0), ..Default::default() };

    let resolved = resolve_limits(&defaults, &overrides, "BTC").expect("own notional limit");

    assert_eq!(resolved.max_order_notional, Some(50.0));
}

#[test]
fn invalid_limits_rejected() {
    let negative = Limits { max_order_qty: Some(-1.0), ..Default::default() };
    assert_eq!(
        code(resolve_limits(&negative, &Limits::default(), "USDT")),
        "invalid_limits"
    );

    let zero_rate = Limits { max_orders_per_sec: Some(0), ..Default::default() };
    assert_eq!(
        code(resolve_limits(&zero_rate, &Limits::default(), "USDT")),
        "invalid_limits"
    );
}
