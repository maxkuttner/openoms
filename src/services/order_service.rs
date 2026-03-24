use axum::http::StatusCode;
use rdkafka::producer::FutureRecord;
use serde_json::json;
use std::time::Duration;
use tracing::{error, info};
use uuid::Uuid;

use crate::adapters;
use crate::db;
use crate::kafka;
use crate::models::{self, EventSource, OrderEventType, OrderRequest};

#[derive(Clone)]
pub struct OrderService {
    account_repo: db::AccountRepo,
    connector_repo: db::ConnectorRepo,
    order_repo: db::OrderRepo,
    alpaca_adapter: adapters::alpaca::AlpacaAdapter,
    kafka: kafka::KafkaClient,
}

impl OrderService {
    pub fn new(
        account_repo: db::AccountRepo,
        connector_repo: db::ConnectorRepo,
        order_repo: db::OrderRepo,
        alpaca_adapter: adapters::alpaca::AlpacaAdapter,
        kafka: kafka::KafkaClient,
    ) -> Self {
        Self {
            account_repo,
            connector_repo,
            order_repo,
            alpaca_adapter,
            kafka,
        }
    }

    async fn require_active_account(
        &self,
        account_id: i64,
    ) -> Result<models::Account, ServiceError> {
        match self.account_repo.get_by_id(account_id).await {
            Ok(Some(account)) if account.is_active => Ok(account),
            Ok(_) => Err(ServiceError::Forbidden("Account is not active")),
            Err(e) => {
                error!(
                    "Database error while fetching account {}: {}",
                    account_id, e
                );
                Err(ServiceError::Internal("Database error"))
            }
        }
    }

    async fn require_connector(
        &self,
        connector_id: i64,
        account_id: i64,
    ) -> Result<models::Connector, ServiceError> {
        match self
            .connector_repo
            .get_by_id(connector_id, account_id)
            .await
        {
            Ok(Some(connector)) => Ok(connector),
            Ok(_) => Err(ServiceError::Forbidden("Connector not found")),
            Err(e) => {
                error!(
                    "Database error while fetching connector {} for account {}: {}",
                    connector_id, account_id, e
                );
                Err(ServiceError::Internal("Database error"))
            }
        }
    }

    async fn forward_to_exchange(
        &self,
        account: models::Account,
        connector: models::Connector,
        order_id: Uuid,
        order: OrderRequest,
    ) -> Result<ServiceResponse, ServiceError> {
        let routed_event = json!({
            "target_provider": connector.provider_code,
            "environment": connector.environment,
        });

        self.order_repo
            .update_order_status(order_id, models::OrderStatus::Routed.as_str(), None, None)
            .await
            .map_err(|e| {
                error!("Failed to mark order {} as routed: {}", order_id, e);
                ServiceError::Internal("Failed to update order status")
            })?;

        self.append_event_safe(
            order_id,
            OrderEventType::OrderRouted,
            models::OrderStatus::Routed,
            &routed_event,
            EventSource::Oms,
        )
        .await;

        if let Some(api_key) = &connector.api_key {
            let api_secret = connector.external_account_id.clone();
            let is_paper = connector.environment == "paper";

            let (status, result_msg, order_status, event_type, external_order_id) = match self
                .alpaca_adapter
                .submit_order(&order, api_key, &api_secret, is_paper)
                .await
            {
                Ok(raw_json) => {
                    info!("Alpaca execution successful");
                    let external_order_id = serde_json::from_str::<serde_json::Value>(&raw_json)
                        .ok()
                        .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_string));

                    self.order_repo
                        .update_order_status(
                            order_id,
                            models::OrderStatus::Acknowledged.as_str(),
                            external_order_id.as_deref(),
                            None,
                        )
                        .await
                        .map_err(|e| {
                            error!("Failed to mark order {} as acknowledged: {}", order_id, e);
                            ServiceError::Internal("Failed to update order status")
                        })?;

                    let event_payload = json!({
                        "provider_response": raw_json,
                        "external_order_id": external_order_id
                    });
                    self.append_event_safe(
                        order_id,
                        OrderEventType::BrokerAck,
                        models::OrderStatus::Acknowledged,
                        &event_payload,
                        EventSource::Alpaca,
                    )
                    .await;

                    (
                        StatusCode::OK,
                        "Order executed".to_string(),
                        models::OrderStatus::Acknowledged,
                        OrderEventType::BrokerAck,
                        external_order_id,
                    )
                }
                Err(e) => {
                    error!("Alpaca execution failed: {}", e);

                    self.order_repo
                        .update_order_status(
                            order_id,
                            models::OrderStatus::Rejected.as_str(),
                            None,
                            Some(&e),
                        )
                        .await
                        .map_err(|db_err| {
                            error!("Failed to mark order {} as rejected: {}", order_id, db_err);
                            ServiceError::Internal("Failed to update order status")
                        })?;

                    let event_payload = json!({ "error": e });
                    self.append_event_safe(
                        order_id,
                        OrderEventType::BrokerReject,
                        models::OrderStatus::Rejected,
                        &event_payload,
                        EventSource::Alpaca,
                    )
                    .await;

                    (
                        StatusCode::BAD_GATEWAY,
                        "Alpaca execution failed".to_string(),
                        models::OrderStatus::Rejected,
                        OrderEventType::BrokerReject,
                        None,
                    )
                }
            };

            self.publish_order_event_to_kafka(
                account.account_id,
                &connector,
                &order,
                order_id,
                order_status.as_str(),
                event_type.as_str(),
                &result_msg,
            )
            .await;

            Ok(ServiceResponse {
                status,
                body: json!({
                    "order_id": order_id,
                    "status": order_status.as_str(),
                    "message": result_msg,
                    "external_order_id": external_order_id,
                }),
            })
        } else {
            let msg = "API Key missing for connector";

            self.order_repo
                .update_order_status(
                    order_id,
                    models::OrderStatus::Rejected.as_str(),
                    None,
                    Some(msg),
                )
                .await
                .map_err(|e| {
                    error!("Failed to mark order {} as rejected: {}", order_id, e);
                    ServiceError::Internal("Failed to update order status")
                })?;

            let event_payload = json!({ "error": msg });
            self.append_event_safe(
                order_id,
                OrderEventType::ValidationReject,
                models::OrderStatus::Rejected,
                &event_payload,
                EventSource::Oms,
            )
            .await;

            self.publish_order_event_to_kafka(
                account.account_id,
                &connector,
                &order,
                order_id,
                models::OrderStatus::Rejected.as_str(),
                OrderEventType::ValidationReject.as_str(),
                msg,
            )
            .await;

            Ok(ServiceResponse {
                status: StatusCode::BAD_REQUEST,
                body: json!({
                    "order_id": order_id,
                    "status": models::OrderStatus::Rejected.as_str(),
                    "message": msg,
                }),
            })
        }
    }

    async fn append_event_safe(
        &self,
        order_id: Uuid,
        event_type: OrderEventType,
        status_after: models::OrderStatus,
        payload: &serde_json::Value,
        source: EventSource,
    ) {
        if let Err(e) = self
            .order_repo
            .append_event(
                order_id,
                event_type.as_str(),
                status_after.as_str(),
                payload,
                source.as_str(),
            )
            .await
        {
            error!(
                "Failed to append {} event for order {}: {}",
                event_type.as_str(),
                order_id,
                e
            );
        }
    }

    async fn publish_order_event_to_kafka(
        &self,
        account_id: i64,
        connector: &models::Connector,
        order: &OrderRequest,
        order_id: Uuid,
        status: &str,
        event_type: &str,
        result: &str,
    ) {
        info!("Publishing order event to Kafka");
        let producer = self.kafka.producer.clone();
        let topic = self.kafka.topic.clone();
        let projector_event_id = format!("order-{}-{}", order_id, event_type);
        let payload = json!({
            "projector_event_id": projector_event_id,
            "order_id": order_id,
            "account_id": account_id,
            "connector_id": connector.connector_id,
            "client_order_id": order.request_id,
            "status": status,
            "event_type": event_type,
            "order": order,
            "result": result,
        });

        let key = format!("order-{}", order_id);
        tokio::spawn(async move {
            let payload = serde_json::to_string(&payload).unwrap_or_default();
            let record = FutureRecord::to(&topic).payload(&payload).key(&key);
            if let Err((err, _)) = producer.send(record, Duration::from_secs(0)).await {
                error!("Failed to publish order event to Kafka: {}", err);
            }
        });
    }
}

pub struct ServiceResponse {
    pub status: StatusCode,
    pub body: serde_json::Value,
}

pub enum ServiceError {
    Forbidden(&'static str),
    Internal(&'static str),
    BadRequest(&'static str),
}
