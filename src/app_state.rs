use std::sync::Arc;
use sqlx::PgPool;

use crate::adapters::BrokerRegistry;

/// Shared application state injected into every Axum handler via State<AppState>.
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    pub admin_token: String,
    pub admin_auth_enabled: bool,
    registry: Arc<BrokerRegistry>,
}

impl AppState {
    pub fn new(pool: PgPool, admin_token: String, admin_auth_enabled: bool, registry: BrokerRegistry) -> Self {
        Self {
            pool,
            admin_token,
            admin_auth_enabled,
            registry: Arc::new(registry),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn registry(&self) -> &Arc<BrokerRegistry> {
        &self.registry
    }
}
