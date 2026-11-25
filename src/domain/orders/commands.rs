use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::state::{OrderSide, OrderStatus, OrderType, TimeInForce};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct SubmitOrder {
    pub order_id: String,
    pub client_order_id: String,
    pub portfolio_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub limit_price: Option<f64>,
    pub quantity: f64,
    /// The roll (or other multi-leg) order this one is a leg of.
    #[serde(default)]
    pub parent_order_id: Option<String>,
    /// The limit is a net spread price (a roll's parent order): any finite
    /// value, zero or negative included. Set by the OMS only (a roll); this
    /// internal command is not in the API schema (no endpoint takes it).
    #[serde(default)]
    pub spread_price: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplaceOrder {
    pub order_id: String,
    pub new_limit_price: Option<f64>,
    pub new_quantity: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct CancelOrder {
    pub order_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuspendOrder {
    pub order_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseOrder {
    pub order_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReceiveExecutionReport {
    pub order_id: String,
    pub report: ExecutionReport,
}

// An execution report is a command you get from a broker or an outside venue.
// We as the OMS try to ingest this command.
// Example: An exchange sends a confirmation that an order was filled or the OMS polls / uses a
// websocket to find out that an order was filled or rejected.
// In any case, the OMS shall the issue an event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionReport {
    Fill {
        execution_id: String,
        fill_qty: f64,
        fill_price: f64,
        venue: String,
    },
    Reject {
        reason: String,
        venue: Option<String>,
    },
    Canceled {
        reason: Option<String>,
        venue: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExpireOrder {
    pub order_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteOrder {
    pub order_id: String,
    pub venue: String,
    pub external_order_id: String,
}

/// An order the OMS first learns about from the venue (placed outside the OMS,
/// or found by recon). Opens the stream in the venue's status; books no fills:
/// `venue_cum_qty` is informational and the booked cum starts at 0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserveOrder {
    pub order_id: String,
    pub client_order_id: String,
    pub portfolio_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub limit_price: Option<f64>,
    pub quantity: f64,
    pub venue: String,
    pub external_order_id: String,
    /// Routed (working) | PartiallyFilled | Filled | Canceled | Expired | Rejected
    /// — a venue "working"/"new" order is observed as `Routed`.
    pub observed_status: OrderStatus,
    pub venue_cum_qty: f64,
    pub venue_created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum OrderCommand {
    SubmitOrder(SubmitOrder),
    RouteOrder(RouteOrder),
    ReplaceOrder(ReplaceOrder),
    CancelOrder(CancelOrder),
    SuspendOrder(SuspendOrder),
    ReleaseOrder(ReleaseOrder),
    ReceiveExecutionReport(ReceiveExecutionReport),
    ExpireOrder(ExpireOrder),
    ObserveOrder(ObserveOrder),
}
