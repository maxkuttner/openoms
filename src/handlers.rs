use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::models::OrderRequest;
use crate::services::order_service::ServiceError;
use crate::AppState;

#[derive(serde::Deserialize)]
pub struct OrderQuery {
    pub account_id: i64,
}

pub enum ApiError {
    Forbidden(&'static str),
    Internal(&'static str),
    BadRequest(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}

impl From<ServiceError> for ApiError {
    fn from(value: ServiceError) -> Self {
        match value {
            ServiceError::Forbidden(msg) => ApiError::Forbidden(msg),
            ServiceError::Internal(msg) => ApiError::Internal(msg),
            ServiceError::BadRequest(msg) => ApiError::BadRequest(msg),
        }
    }
}

pub async fn health() -> &'static str {
    "OK"
}

pub async fn create_order(
    Query(query): Query<OrderQuery>,
    State(state): State<Arc<AppState>>,
    Json(order): Json<OrderRequest>,
) -> Result<Response, ApiError> {
    let service_response = state
        .order_service
        .create_order(query.account_id, order)
        .await
        .map_err(ApiError::from)?;

    Ok((service_response.status, Json(service_response.body)).into_response())
}
