use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fees::FeeEffect;
use super::state::{OrderSide, OrderStatus, OrderType, TimeInForce};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderEventType {
    OrderSubmitted,
    OrderRejected,
    OrderRouted,
    OrderAmended,
    OrderCanceled,
    CancelRejected,
    OrderPartiallyFilled,
    OrderFilled,
    OrderExpired,
    OrderSuspended,
    OrderReleased,
    OrderFeeCharged,
    OrderObserved,
    OrderLegsFilled,
}

impl OrderEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrderSubmitted => "order_submitted",
            Self::OrderRejected => "order_rejected",
            Self::OrderRouted => "order_routed",
            Self::OrderAmended => "order_amended",
            Self::OrderCanceled => "order_canceled",
            Self::CancelRejected => "cancel_rejected",
            Self::OrderPartiallyFilled => "order_partially_filled",
            Self::OrderFilled => "order_filled",
            Self::OrderExpired => "order_expired",
            Self::OrderSuspended => "order_suspended",
            Self::OrderReleased => "order_released",
            Self::OrderFeeCharged => "order_fee_charged",
            Self::OrderObserved => "order_observed",
            Self::OrderLegsFilled => "order_legs_filled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum OrderEventPayload {
    OrderSubmitted {
        client_order_id: String,
        portfolio_id: String,
        account_id: String,
        instrument_id: String,
        side: OrderSide,
        order_type: OrderType,
        time_in_force: TimeInForce,
        limit_price: Option<f64>,
        quantity: f64,
        /// Set on a leg of a multi-leg order (a futures roll): the parent order.
        /// Absent on every ordinary order, so their payload is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_order_id: Option<String>,
    },
    OrderRejected {
        reason: String,
    },
    OrderRouted {
        venue: String,
        external_order_id: String,
    },
    OrderAmended {
        previous_limit_price: Option<f64>,
        new_limit_price: Option<f64>,
        previous_quantity: f64,
        new_quantity: f64,
    },
    OrderCanceled {
        reason: Option<String>,
    },
    CancelRejected {
        reason: String,
    },
    OrderPartiallyFilled {
        execution_id: String,
        fill_qty: f64,
        fill_price: f64,
        cum_qty: f64,
        leaves_qty: f64,
        avg_px: f64,
        venue: String,
    },
    OrderFilled {
        execution_id: String,
        fill_qty: f64,
        fill_price: f64,
        cum_qty: f64,
        leaves_qty: f64,
        avg_px: f64,
        venue: String,
    },
    OrderExpired {
        reason: Option<String>,
    },
    OrderSuspended {
        reason: Option<String>,
        resume_to_status: OrderStatus,
    },
    OrderReleased {
        resumed_to_status: OrderStatus,
    },
    OrderFeeCharged {
        fee_id: String,
        execution_id: Option<String>,
        amount: f64,
        currency: String,
        effect: FeeEffect,
    },
    /// A multi-leg (parent) order's progress, derived from its leg orders'
    /// fills. Status-only: not a fill (positions and the fills topic read
    /// `order_partially_filled` / `order_filled` only).
    OrderLegsFilled {
        cum_qty: f64,
        leaves_qty: f64,
        /// Net price per unit: buy-leg average prices minus sell-leg ones.
        avg_px: f64,
    },
    /// Stream opened from the venue's view of an order; books no fills.
    OrderObserved {
        client_order_id: String,
        portfolio_id: String,
        account_id: String,
        instrument_id: String,
        side: OrderSide,
        order_type: OrderType,
        time_in_force: TimeInForce,
        limit_price: Option<f64>,
        quantity: f64,
        venue: String,
        external_order_id: String,
        observed_status: OrderStatus,
        venue_cum_qty: f64,
        venue_created_at: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderDomainEvent {
    pub event_id: String,
    pub event_type: OrderEventType,
    pub order_id: String,
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    pub payload: OrderEventPayload,
    pub version: i64,
    pub status_after: OrderStatus,
    /// When it happened at the venue (trade time for fills); None when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
}
