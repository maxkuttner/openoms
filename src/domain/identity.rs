use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Serialize, FromRow, Clone, utoipa::ToSchema)]
pub struct Principal {
    pub id: Uuid,
    pub code: String,
    pub principal_type: String,
    pub external_subject: Option<String>,
    pub display_name: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow, Clone, utoipa::ToSchema)]
pub struct Book {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub status: String,
    pub base_currency: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow, Clone, utoipa::ToSchema)]
pub struct Account {
    pub id: Uuid,
    pub code: String,
    pub broker_code: String,
    pub environment: String,
    pub external_account_ref: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
