use crate::{Exposure, Limits, OrderInput, Reject, RiskInput, Side, TradingState, VenueRules, USD_LIKE};

/// Evaluate one order. Pure: no clock, no I/O, no mutation. The caller owns
/// the rate window and any reservation bookkeeping.
pub fn evaluate(input: &RiskInput) -> Result<(), Reject> {
    validate_limits(&input.limits)?;
    validate_exposure(&input.exposure)?;
    validate_order(&input.order)?;
    validate_valuation(input)?;

    match input.state {
        TradingState::Halted => {
            return Err(Reject::new("trading_halted", "trading is halted"));
        }
        TradingState::Reducing => check_reducing(input)?,
        TradingState::Active => {}
    }

    let limits = &input.limits;
    let order = &input.order;

    if let Some(max) = limits.max_order_qty {
        if order.quantity > max {
            return Err(Reject::new(
                "max_order_qty",
                format!("order qty {} > {max}", order.quantity),
            ));
        }
    }

    if let Some(max) = limits.max_order_notional {
        if let Some(price) = order.limit_price.or(input.valuation.order_price) {
            let notional = order.quantity * price * order.multiplier;
            if notional > max {
                return Err(Reject::new(
                    "max_order_notional",
                    format!("order notional {notional} > {max}"),
                ));
            }
        }
    }

    let projected_abs = projected_abs(input);

    if let Some(max) = limits.max_position_qty {
        if projected_abs > max {
            return Err(Reject::new(
                "max_position_qty",
                format!("projected position {projected_abs} > {max}"),
            ));
        }
    }

    if let (Some(max), Some(price)) = (limits.max_position_notional, input.valuation.position_price) {
        let notional = projected_abs * price * order.multiplier;
        if notional > max {
            return Err(Reject::new(
                "max_position_notional",
                format!("projected position notional {notional} > {max}"),
            ));
        }
    }

    if let (Some(band), Some(limit), Some(reference)) = (
        limits.price_band_pct,
        order.limit_price,
        input.valuation.reference_price,
    ) {
        let off = (limit - reference).abs() / reference * 100.0;
        if off > band * (1.0 + 1e-9) {
            return Err(Reject::new(
                "price_band",
                format!("price {limit} is {off:.2}% from {reference} (band {band}%)"),
            ));
        }
    }

    Ok(())
}

/// Refuses an order when a configured limit needs a price that is missing.
/// `evaluate` skips such a limit instead (the OMS lets a market order without
/// a mark through its notional check); callers that must not trade unchecked,
/// like the co-located runtime, call this first.
pub fn require_prices(input: &RiskInput) -> Result<(), Reject> {
    let limits = &input.limits;
    let order = &input.order;
    if limits.max_order_notional.is_some() && order.limit_price.or(input.valuation.order_price).is_none() {
        return Err(missing_price("max_order_notional", "an order price"));
    }
    if limits.max_position_notional.is_some() && input.valuation.position_price.is_none() {
        return Err(missing_price("max_position_notional", "a position price"));
    }
    // The band bounds a limit price; a market order has none to bound.
    if limits.price_band_pct.is_some() && order.limit_price.is_some() && input.valuation.reference_price.is_none() {
        return Err(missing_price("price_band", "a reference price"));
    }
    Ok(())
}

fn missing_price(limit: &str, needs: &str) -> Reject {
    Reject::new("missing_price", format!("{limit} is configured but there is no {needs}"))
}

/// Resolve per-field limits: an override wins only where it is configured;
/// otherwise the default applies. A notional default is denominated in USD:
/// inherited into a non-USD-like quote it is refused, unless the venue is
/// `per_currency_notional`, where it is dropped (no notional check) instead of
/// being applied to the wrong currency. An explicit override may use any
/// quote. A `band_exempt` venue resolves no price band.
pub fn resolve_limits(
    defaults: &Limits,
    overrides: &Limits,
    quote_currency: &str,
    rules: VenueRules,
) -> Result<Limits, Reject> {
    let usd_like = USD_LIKE.contains(&quote_currency);
    let inherited_notional = overrides.max_order_notional.is_none() && defaults.max_order_notional.is_some()
        || overrides.max_position_notional.is_none() && defaults.max_position_notional.is_some();
    if inherited_notional && !usd_like && !rules.per_currency_notional {
        return Err(Reject::new(
            "no_notional_limit",
            format!("no notional limit for quote currency {quote_currency}"),
        ));
    }
    let notional = |own: Option<f64>, default: Option<f64>| own.or(if usd_like { default } else { None });

    let resolved = Limits {
        max_order_qty: overrides.max_order_qty.or(defaults.max_order_qty),
        max_order_notional: notional(overrides.max_order_notional, defaults.max_order_notional),
        max_position_qty: overrides.max_position_qty.or(defaults.max_position_qty),
        max_position_notional: notional(overrides.max_position_notional, defaults.max_position_notional),
        price_band_pct: if rules.band_exempt { None } else { overrides.price_band_pct.or(defaults.price_band_pct) },
        max_orders_per_sec: overrides.max_orders_per_sec.or(defaults.max_orders_per_sec),
    };
    validate_limits(&resolved)?;
    Ok(resolved)
}

fn check_reducing(input: &RiskInput) -> Result<(), Reject> {
    let position = input.exposure.position_qty;
    let (reduces, remaining, reserved) = match input.order.side {
        Side::Buy => (
            position < 0.0,
            (-position).max(0.0),
            input.exposure.working_buy_qty,
        ),
        Side::Sell => (
            position > 0.0,
            position.max(0.0),
            input.exposure.working_sell_qty,
        ),
    };

    if !reduces {
        return Err(Reject::new(
            "trading_reducing",
            "trading state reducing; order does not reduce the current position",
        ));
    }
    if reserved + input.order.quantity > remaining {
        return Err(Reject::new(
            "trading_reducing",
            format!(
                "reducing order {} plus working {reserved} crosses flat from {position}",
                input.order.quantity
            ),
        ));
    }
    Ok(())
}

/// The largest absolute position reachable by the candidate. Working orders on
/// the opposite side are assumed not to fill; the candidate only contributes to
/// its own direction.
fn projected_abs(input: &RiskInput) -> f64 {
    let position = input.exposure.position_qty;
    let qty = input.order.quantity;
    let (long, short) = match input.order.side {
        Side::Buy => (
            position + input.exposure.working_buy_qty + qty,
            position - input.exposure.working_sell_qty,
        ),
        Side::Sell => (
            position + input.exposure.working_buy_qty,
            position - input.exposure.working_sell_qty - qty,
        ),
    };
    long.abs().max(short.abs())
}

fn validate_limits(limits: &Limits) -> Result<(), Reject> {
    let bad = [
        limits.max_order_qty,
        limits.max_order_notional,
        limits.max_position_qty,
        limits.max_position_notional,
        limits.price_band_pct,
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.is_finite() || value < 0.0);

    if bad || limits.max_orders_per_sec == Some(0) {
        return Err(Reject::new("invalid_limits", "risk limits must be finite and non-negative"));
    }
    Ok(())
}

fn validate_exposure(exposure: &Exposure) -> Result<(), Reject> {
    let finite = exposure.position_qty.is_finite()
        && exposure.working_buy_qty.is_finite()
        && exposure.working_sell_qty.is_finite();
    let non_negative = exposure.working_buy_qty >= 0.0 && exposure.working_sell_qty >= 0.0;
    if !finite || !non_negative {
        return Err(Reject::new(
            "invalid_exposure",
            "position must be finite and working quantities non-negative",
        ));
    }
    Ok(())
}

fn validate_order(order: &OrderInput) -> Result<(), Reject> {
    if !order.quantity.is_finite() || order.quantity <= 0.0 {
        return Err(Reject::new(
            "invalid_quantity",
            format!("order quantity must be finite and positive, got {}", order.quantity),
        ));
    }
    if !order.multiplier.is_finite() || order.multiplier <= 0.0 {
        return Err(Reject::new(
            "invalid_multiplier",
            format!("multiplier must be finite and positive, got {}", order.multiplier),
        ));
    }
    if let Some(price) = order.limit_price {
        validate_price(price, "invalid_price", "limit price")?;
    }
    Ok(())
}

fn validate_valuation(input: &RiskInput) -> Result<(), Reject> {
    if let Some(price) = input.valuation.order_price {
        validate_price(price, "invalid_price", "order valuation price")?;
    }
    if let Some(price) = input.valuation.position_price {
        validate_price(price, "invalid_price", "position valuation price")?;
    }
    if input.limits.price_band_pct.is_some() {
        if let Some(reference) = input.valuation.reference_price {
            validate_price(reference, "invalid_reference_price", "reference price")?;
        }
    }
    Ok(())
}

fn validate_price(price: f64, code: &str, label: &str) -> Result<(), Reject> {
    if !price.is_finite() || price <= 0.0 {
        return Err(Reject::new(
            code,
            format!("{label} must be finite and positive, got {price}"),
        ));
    }
    Ok(())
}
