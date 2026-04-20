use std::env;
use std::time::Duration;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::producer::{FutureProducer, FutureRecord};

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

        let orders_topic = match env::var("KAFKA_TOPIC") {
            Ok(v) if !v.is_empty() => v,
            _ => return Err("KAFKA_TOPIC is not set".to_string()),
        };

        let client_id = env::var("KAFKA_CLIENT_ID").unwrap_or_else(|_| "rust-oms".to_string());
        let projector_group_id = env::var("KAFKA_PROJECTOR_GROUP_ID")
            .unwrap_or_else(|_| "position-projector-v1".to_string());

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

        match client.producer.send(record, Duration::from_secs(5)).await {
            Ok((partition, offset)) => {
                tracing::info!(
                    order_id = %order_id,
                    event_type = %event.event_type.as_str(),
                    partition,
                    offset,
                    "published event to Kafka"
                );
            }
            Err((err, _msg)) => {
                tracing::error!(
                    order_id = %order_id,
                    event_type = %event.event_type.as_str(),
                    error = ?err,
                    "failed to publish event to Kafka (non-fatal)"
                );
            }
        }
    }
}
