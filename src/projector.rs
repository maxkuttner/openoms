use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::message::Message;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::db;
use crate::kafka;
use crate::models;

pub async fn run_position_projector(
    pool: PgPool,
    kafka_config: kafka::KafkaConfig,
) -> Result<(), String> {
    let consumer: StreamConsumer = kafka_config
        .create_projector_consumer()
        .map_err(|e| format!("Failed to create projector consumer: {e}"))?;

    let repo = db::PositionProjectorRepo::new(pool);
    info!(
        topic = kafka_config.orders_topic,
        group_id = kafka_config.projector_group_id,
        "Position projector started"
    );

    loop {
        let msg = match consumer.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!(error = %e, "Projector consumer recv failed");
                continue;
            }
        };

        let payload = match msg.payload() {
            Some(v) => v,
            None => {
                warn!("Skipping empty Kafka payload");
                let _ = consumer.commit_message(&msg, CommitMode::Async);
                continue;
            }
        };

        let event: models::KafkaOrderEvent = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Skipping invalid order event payload");
                let _ = consumer.commit_message(&msg, CommitMode::Async);
                continue;
            }
        };

        if event.event_type != "broker_ack" {
            let _ = consumer.commit_message(&msg, CommitMode::Async);
            continue;
        }

        let Some(fill_price) = event.order.price else {
            warn!(
                order_id = event.order_id,
                "Skipping broker_ack without fill price"
            );
            let _ = consumer.commit_message(&msg, CommitMode::Async);
            continue;
        };

        let projector_event_id = event
            .projector_event_id
            .unwrap_or_else(|| format!("order-{}-{}", event.order_id, event.event_type));

        if let Err(e) = repo
            .apply_ack_fill_projection(
                &projector_event_id,
                event.order_id,
                event.account_id,
                event.connector_id,
                event.order.side.as_str(),
                event.order.quantity,
                fill_price,
            )
            .await
        {
            error!(
                error = %e,
                order_id = event.order_id,
                projector_event_id = %projector_event_id,
                "Projector apply failed"
            );
            continue;
        }

        if let Err(e) = consumer.commit_message(&msg, CommitMode::Async) {
            warn!(error = %e, "Failed to commit projector offset");
        }
    }
}
