use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;


#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OrderRequest {
    pub request_id: String, // Crucial for TUI tracking
    pub connector_id: i64,
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub quantity: f64,
    pub price: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct KafkaOrderEvent {
    pub projector_event_id: Option<String>,
    pub order_id: Uuid,
    pub account_id: i64,
    pub connector_id: i64,
    pub client_order_id: String,
    pub status: String,
    pub event_type: String,
    pub order: OrderRequest,
    pub result: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::Type)]
#[serde(rename_all = "lowercase")] // <--- THIS IS THE KEY
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")] // <--- AND THIS
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text")]
pub enum OrderStatus {
    Received,
    Validated,
    Routed,
    Acknowledged,
    PartiallyFilled,
    Filled,
    Rejected,
    Cancelled,
    Expired,
    Failed,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Validated => "validated",
            Self::Routed => "routed",
            Self::Acknowledged => "acknowledged",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum OrderEventType {
    OrderReceived,
    OrderValidated,
    OrderRouted,
    BrokerAck,
    BrokerReject,
    ValidationReject,
    MigrationImport,
    SeedImport,
}

impl OrderEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OrderReceived => "order_received",
            Self::OrderValidated => "order_validated",
            Self::OrderRouted => "order_routed",
            Self::BrokerAck => "broker_ack",
            Self::BrokerReject => "broker_reject",
            Self::ValidationReject => "validation_reject",
            Self::MigrationImport => "migration_import",
            Self::SeedImport => "seed_import",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum EventSource {
    Oms,
    Alpaca,
    Migration,
    Seed,
}

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Oms => "oms",
            Self::Alpaca => "alpaca",
            Self::Migration => "migration",
            Self::Seed => "seed",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
pub struct Order {
    pub order_id: Uuid,
    pub account_id: i64,
    pub connector_id: i64,
    pub instrument_id: i64,
    pub client_order_id: String,
    pub side: String,
    pub order_type: String,
    pub quantity: f64,
    pub limit_price: Option<f64>,
    pub filled_quantity: f64,
    pub status: String,
    pub external_order_id: Option<String>,
    pub error_message: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
#[allow(dead_code)]
pub struct OrderEvent {
    pub event_id: i64,
    pub order_id: Uuid,
    pub event_type: String,
    pub status_after: String,
    pub payload_json: serde_json::Value,
    pub source: String,
    pub created_at: NaiveDateTime,
}
