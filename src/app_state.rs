use sqlx::PgPool;

// AppState to provide stores and tools to various handlers
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
}

impl AppState {
    // constructor
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // pool getter
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
