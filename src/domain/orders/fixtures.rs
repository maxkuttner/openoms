use chrono::Utc;

use super::aggregate::{EventMetadata, OrderAggregate};
use super::commands::{
    CancelOrder, ExecutionReport, ExpireOrder, ReceiveExecutionReport, ReleaseOrder, ReplaceOrder,
    SubmitOrder, SuspendOrder,
};
use super::state::{OrderSide, OrderType, TimeInForce};

#[derive(Debug, Clone)]
pub struct OrderTestData {
    pub order_id: String,
    pub client_order_id: String,
    pub book_id: String,
    pub account_id: String,
    pub instrument_id: String,
}

impl OrderTestData {
    pub fn new(order_id: impl Into<String>) -> Self {
        let order_id = order_id.into();
        Self {
            client_order_id: format!("c-{order_id}"),
            book_id: "b-1".to_string(),
            account_id: "a-1".to_string(),
            instrument_id: "i-1".to_string(),
            order_id,
        }
    }

    pub fn submit(
        &self,
        side: OrderSide,
        order_type: OrderType,
        time_in_force: TimeInForce,
        limit_price: Option<f64>,
        quantity: f64,
    ) -> SubmitOrder {
        SubmitOrder {
            order_id: self.order_id.clone(),
            client_order_id: self.client_order_id.clone(),
            book_id: self.book_id.clone(),
            account_id: self.account_id.clone(),
            instrument_id: self.instrument_id.clone(),
            side,
            order_type,
            time_in_force,
            limit_price,
            quantity,
        }
    }

    pub fn submit_limit(&self, qty: f64, px: f64) -> SubmitOrder {
        self.submit(
            OrderSide::Buy,
            OrderType::Limit,
            TimeInForce::Day,
            Some(px),
            qty,
        )
    }

    pub fn replace(&self, new_limit_price: Option<f64>, new_quantity: Option<f64>) -> ReplaceOrder {
        ReplaceOrder {
            order_id: self.order_id.clone(),
            new_limit_price,
            new_quantity,
        }
    }

    pub fn cancel(&self, reason: Option<&str>) -> CancelOrder {
        CancelOrder {
            order_id: self.order_id.clone(),
            reason: reason.map(|s| s.to_string()),
        }
    }

    pub fn suspend(&self, reason: Option<&str>) -> SuspendOrder {
        SuspendOrder {
            order_id: self.order_id.clone(),
            reason: reason.map(|s| s.to_string()),
        }
    }

    pub fn release(&self) -> ReleaseOrder {
        ReleaseOrder {
            order_id: self.order_id.clone(),
        }
    }

    pub fn expire(&self, reason: Option<&str>) -> ExpireOrder {
        ExpireOrder {
            order_id: self.order_id.clone(),
            reason: reason.map(|s| s.to_string()),
        }
    }

    pub fn exec_fill(
        &self,
        exec_id: &str,
        qty: f64,
        px: f64,
        venue: &str,
    ) -> ReceiveExecutionReport {
        ReceiveExecutionReport {
            order_id: self.order_id.clone(),
            report: ExecutionReport::Fill {
                execution_id: exec_id.to_string(),
                fill_qty: qty,
                fill_price: px,
                venue: venue.to_string(),
            },
        }
    }

    pub fn exec_reject(&self, reason: &str, venue: Option<&str>) -> ReceiveExecutionReport {
        ReceiveExecutionReport {
            order_id: self.order_id.clone(),
            report: ExecutionReport::Reject {
                reason: reason.to_string(),
                venue: venue.map(|v| v.to_string()),
            },
        }
    }
}

pub fn metadata(event_id: &str) -> EventMetadata {
    EventMetadata {
        event_id: event_id.to_string(),
        timestamp: Utc::now(),
        actor: "test".to_string(),
    }
}

pub fn submitted_aggregate(t: &OrderTestData) -> OrderAggregate {
    use super::commands::OrderCommand;

    let aggregate = OrderAggregate::default();
    let events = aggregate
        .decide(
            OrderCommand::SubmitOrder(t.submit_limit(10.0, 100.0)),
            metadata("e-1"),
        )
        .expect("submit should succeed");

    OrderAggregate::rehydrate(&events).expect("rehydrate should succeed")
}
