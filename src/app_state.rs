use std::sync::Arc;
use sqlx::PgPool;
use symbology::{Identifier, InMemoryCache, OpenFigiClient};

use crate::adapters::BrokerRegistry;
use crate::kafka::KafkaClient;
use crate::marks::MarkStore;
use crate::stream_health::StreamHealthRegistry;

/// The OpenFIGI-backed instrument identification engine, with an in-memory cache.
pub type SymbologyEngine = Identifier<OpenFigiClient, InMemoryCache>;

/// Shared application state injected into every Axum handler via State<AppState>.
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    pub admin_token: String,
    pub admin_auth_enabled: bool,
    registry: Arc<BrokerRegistry>,
    kafka: Option<KafkaClient>,
    symbology: Arc<SymbologyEngine>,
    stream_health: StreamHealthRegistry,
    marks: MarkStore,
}

impl AppState {
    pub fn new(
        pool: PgPool,
        admin_token: String,
        admin_auth_enabled: bool,
        registry: BrokerRegistry,
        kafka: Option<KafkaClient>,
        symbology: SymbologyEngine,
    ) -> Self {
        Self {
            pool,
            admin_token,
            admin_auth_enabled,
            registry: Arc::new(registry),
            kafka,
            symbology: Arc::new(symbology),
            stream_health: StreamHealthRegistry::new(),
            marks: MarkStore::new(),
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

    pub fn symbology(&self) -> &Arc<SymbologyEngine> {
        &self.symbology
    }

    pub fn stream_health(&self) -> &StreamHealthRegistry {
        &self.stream_health
    }

    pub fn marks(&self) -> &MarkStore { &self.marks }
}
