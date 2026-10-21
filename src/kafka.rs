use std::env;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::producer::{FutureProducer, FutureRecord};
use tracing::info;

#[derive(Clone)]
pub struct KafkaConfig {
    pub brokers: String,
    pub client_id: String,
    pub orders_topic: String,
    pub projector_group_id: String,
}

impl KafkaConfig {
    pub fn from_env() -> Result<Self, String> {
        let brokers = match env::var("KAFKA_BROKER") {
            Ok(v) if !v.is_empty() => v,
            _ => return Err("KAFKA_BROKER is not set".to_string()),
        };
        info!("KAFKA_BROKER: {}", brokers);

        let orders_topic = match env::var("KAFKA_TOPIC") {
            Ok(v) if !v.is_empty() => v,
            _ => return Err("KAFKA_TOPIC is not set".to_string()),
        };
        info!("KAFKA_TOPIC: {}", orders_topic);

        let client_id = env::var("KAFKA_CLIENT_ID").
            unwrap_or_else(|_| "rust-oms".to_string());
        info!("KAFKA_CLIENT_ID: {}", client_id);

        let projector_group_id = env::var("KAFKA_PROJECTOR_GROUP_ID")
            .unwrap_or_else(|_| "position-projector-v1".to_string());
        info!("KAFKA_PROJECTOR_GROUP_ID: {}", projector_group_id);

        Ok(Self {
            brokers,
            client_id,
            orders_topic,
            projector_group_id,
        })
    }

    fn base_client_config(&self, client_id: &str) -> ClientConfig {
        let mut config = ClientConfig::new();
        config
            .set("bootstrap.servers", &self.brokers)
            .set("client.id", client_id)
            .set("broker.address.family", "v4");
        config
    }

    pub fn create_producer_client(&self) -> Result<KafkaClient, KafkaError> {
        KafkaClient::new(self)
    }
    
    // return a projector
    pub fn create_projector_consumer(&self) -> Result<StreamConsumer, KafkaError> {
        let consumer_client_id = format!("{}-projector", self.client_id);
        let consumer: StreamConsumer = self
            .base_client_config(&consumer_client_id)
            .set("group.id", &self.projector_group_id)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "latest")
            .create()?;

        consumer.subscribe(&[&self.orders_topic])?;
        Ok(consumer)
    }
}

// Kafka client to hold broker connections and produce/publish events
#[derive(Clone)]
pub struct KafkaClient {
    pub producer: FutureProducer,
    pub topic: String,
}

impl KafkaClient {
    pub fn new(config: &KafkaConfig) -> Result<Self, KafkaError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &config.brokers)
            .set("client.id", &config.client_id)
            .set("retries", "5")
            .set("acks", "all")
            .set("broker.address.family", "v4")
            .create()?;

        Ok(Self {
            producer,
            topic: config.orders_topic.clone(),
        })
    }
}

pub async fn publish_events(
    client: Option<&KafkaClient>,
    order_id: &str,
    events: &[crate::domain::orders::events::OrderDomainEvent],
) {
    let client = match client {
        Some(c) => c,
        None => return,
    };

    for event in events {
        let payload = match serde_json::to_string(event) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(
                    order_id = %order_id,
                    event_type = %event.event_type.as_str(),
                    error = ?err,
                    "failed to serialize domain event for Kafka"
                );
                continue;
            }
        };

        let record = FutureRecord::to(&client.topic)
            .key(order_id)
            .payload(payload.as_str());

        // Enqueue without awaiting delivery. `send(..).await` waits until the
        // broker acks or librdkafka's `message.timeout.ms` (300s) expires, and
        // every caller — order submit, cancel, fills off the broker streams —
        // sits on that await, so a down Kafka stalled each order ~5 minutes
        // before it reached the broker. Publishing is non-fatal; the delivery
        // outcome is only logged, from a detached task. Enqueueing here, in
        // call order, keeps the per-order event order on the partition.
        let event_type = event.event_type.as_str();
        match client.producer.send_result(record) {
            Ok(delivery) => {
                let order_id = order_id.to_string();
                tokio::spawn(async move {
                    match delivery.await {
                        Ok(Ok((partition, offset))) => tracing::info!(
                            order_id = %order_id,
                            event_type = %event_type,
                            partition,
                            offset,
                            "published event to Kafka"
                        ),
                        Ok(Err((err, _msg))) => tracing::error!(
                            order_id = %order_id,
                            event_type = %event_type,
                            error = ?err,
                            "failed to publish event to Kafka (non-fatal)"
                        ),
                        Err(_canceled) => tracing::error!(
                            order_id = %order_id,
                            event_type = %event_type,
                            "Kafka producer dropped before delivery (non-fatal)"
                        ),
                    }
                });
            }
            Err((err, _record)) => {
                tracing::error!(
                    order_id = %order_id,
                    event_type = %event_type,
                    error = ?err,
                    "failed to enqueue event for Kafka (non-fatal)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload, OrderEventType};
    use crate::domain::orders::state::OrderStatus;

    /// Regression: with the broker unreachable, `publish_events` used to await
    /// delivery until librdkafka's `message.timeout.ms` (300s default) — and
    /// every caller (order submit, cancel, fills off the broker streams) sits
    /// on that await, so a down Kafka stalled each order ~5 minutes before it
    /// reached the broker. Publishing is non-fatal by design; it must not wait.
    #[tokio::test]
    async fn publish_does_not_wait_for_an_unreachable_broker() {
        let config = KafkaConfig {
            brokers: "127.0.0.1:1".to_string(),
            client_id: "publish-test".to_string(),
            orders_topic: "orders".to_string(),
            projector_group_id: "unused".to_string(),
        };
        let client = KafkaClient::new(&config).expect("producer builds without a broker");
        let event = OrderDomainEvent {
            event_id: "e-1".to_string(),
            event_type: OrderEventType::OrderCanceled,
            order_id: "o-1".to_string(),
            timestamp: chrono::Utc::now(),
            actor: "test".to_string(),
            payload: OrderEventPayload::OrderCanceled { reason: None },
            version: 1,
            status_after: OrderStatus::Canceled,
        };

        let started = std::time::Instant::now();
        publish_events(Some(&client), "o-1", &[event.clone(), event]).await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "publish_events blocked for {:?}",
            started.elapsed()
        );
    }
}
