use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum OrderEventPayload {
    OrderSubmitted {
        client_order_id: String,
        account_id: String,
        instrument_id: String,
        side: OrderSide,
        order_type: OrderType,
        time_in_force: TimeInForce,
        limit_price: Option<f64>,
        quantity: f64,
    },
    OrderRejected {
        reason: String,
    },
    OrderRouted {
        venue: String,
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
}
