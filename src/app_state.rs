use sqlx::PgPool;

use crate::auth::AuthConfig;

// AppState to provide stores and tools to various handlers
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    pub auth: AuthConfig,
}

impl AppState {
    // constructor
    pub fn new(pool: PgPool, auth: AuthConfig) -> Self {
        Self { pool, auth }
    }

    // pool getter
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
