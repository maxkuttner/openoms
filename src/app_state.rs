use std::sync::Arc;
use sqlx::PgPool;

use crate::adapters::BrokerRegistry;
use crate::kafka::KafkaClient;

/// Shared application state injected into every Axum handler via State<AppState>.
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    pub admin_token: String,
    pub admin_auth_enabled: bool,
    registry: Arc<BrokerRegistry>,
    kafka: Option<KafkaClient>,
}

impl AppState {
    pub fn new(pool: PgPool, admin_token: String, admin_auth_enabled: bool, registry: BrokerRegistry, kafka: Option<KafkaClient>) -> Self {
        Self {
            pool,
            admin_token,
            admin_auth_enabled,
            registry: Arc::new(registry),
            kafka,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn registry(&self) -> &Arc<BrokerRegistry> {
        &self.registry
    }

    pub fn kafka(&self) -> Option<&KafkaClient> {
        self.kafka.as_ref()
    }
}
