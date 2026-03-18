use serde::{Deserialize, Serialize};

use super::state::{OrderSide, OrderType, TimeInForce};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubmitOrder {
    pub order_id: String,
    pub client_order_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub limit_price: Option<f64>,
    pub quantity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplaceOrder {
    pub order_id: String,
    pub new_limit_price: Option<f64>,
    pub new_quantity: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExpireOrder {
    pub order_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum OrderCommand {
    SubmitOrder(SubmitOrder),
    ReplaceOrder(ReplaceOrder),
    CancelOrder(CancelOrder),
    SuspendOrder(SuspendOrder),
    ReleaseOrder(ReleaseOrder),
    ReceiveExecutionReport(ReceiveExecutionReport),
    ExpireOrder(ExpireOrder),
}
