use std::env;

use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::error::KafkaError;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::util::Timeout;
use tracing::info;

use crate::kafka_outbox::{OutboxRecord, OutboxSink, OutboxTopics};

#[derive(Clone)]
pub struct KafkaConfig {
    pub brokers: String,
    pub client_id: String,
    pub orders_topic: String,
    pub fills_topic: String,
    pub positions_topic: String,
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

        let fills_topic = or_default(env::var("KAFKA_FILLS_TOPIC"), "oms.fills");
        info!("KAFKA_FILLS_TOPIC: {}", fills_topic);

        let positions_topic = or_default(env::var("KAFKA_POSITIONS_TOPIC"), "oms.positions");
        info!("KAFKA_POSITIONS_TOPIC: {}", positions_topic);

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
            fills_topic,
            positions_topic,
            projector_group_id,
        })
    }

    pub fn topics(&self) -> OutboxTopics {
        OutboxTopics {
            orders: self.orders_topic.clone(),
            fills: self.fills_topic.clone(),
            positions: self.positions_topic.clone(),
        }
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

// an unset or blank variable means the default
fn or_default(value: Result<String, env::VarError>, default: &str) -> String {
    match value {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

// Kafka client to hold broker connections and produce/publish events
#[derive(Clone)]
pub struct KafkaClient {
    pub producer: FutureProducer,
}

impl KafkaClient {
    pub fn new(config: &KafkaConfig) -> Result<Self, KafkaError> {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", &config.brokers)
            .set("client.id", &config.client_id)
            .set("retries", "5")
            .set("acks", "all")
            // bounds how long the outbox bridge waits on one record while the
            // broker is down before it backs off and retries
            .set("message.timeout.ms", "30000")
            .set("broker.address.family", "v4")
            .create()?;

        Ok(Self { producer })
    }
}

/// The outbox bridge's way out: one record, awaited until the broker acks it.
pub struct KafkaSink(pub KafkaClient);

#[async_trait::async_trait]
impl OutboxSink for KafkaSink {
    async fn send(&self, record: &OutboxRecord) -> Result<(), String> {
        let r = FutureRecord::to(&record.topic).key(&record.key).payload(&record.payload);
        self.0
            .producer
            .send(r, Timeout::Never)
            .await
            .map(|_| ())
            .map_err(|(e, _)| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.env.example` ships the topic variables blank; blank must mean "default",
    /// not a topic named "".
    #[test]
    fn blank_topic_falls_back_to_default() {
        assert_eq!(or_default(Ok(String::new()), "oms.fills"), "oms.fills");
        assert_eq!(or_default(Err(env::VarError::NotPresent), "oms.fills"), "oms.fills");
        assert_eq!(or_default(Ok("custom".into()), "oms.fills"), "custom");
    }
}
