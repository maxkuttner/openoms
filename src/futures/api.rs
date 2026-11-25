//! `GET /futures/roots`: every active root with its front and next contract.

use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;

use super::front::{front_and_next, venue_today, FutureContract};
use crate::app_state::AppState;
use crate::handlers::ApiError;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FutureRootView {
    pub code: String,
    pub name: String,
    pub venue: String,
    pub front: Option<FutureContract>,
    pub next: Option<FutureContract>,
}

#[utoipa::path(
    get, path = "/futures/roots", tag = "orders",
    responses(
        (status = 200, description = "Active futures roots with front and next contract", body = [FutureRootView]),
        (status = 401, description = "No credential"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_roots(State(state): State<AppState>) -> Result<Json<Vec<FutureRootView>>, ApiError> {
    let fail = |e: sqlx::Error| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list futures roots: {e:?}"),
    };
    let roots: Vec<(String, String, String)> =
        sqlx::query_as("SELECT code, name, venue FROM future_root WHERE status = 'ACTIVE' ORDER BY code")
            .fetch_all(state.pool())
            .await
            .map_err(fail)?;
    let today = venue_today(chrono::Utc::now());
    let mut out = Vec::with_capacity(roots.len());
    for (code, name, venue) in roots {
        let (front, next) = front_and_next(state.pool(), &code, today).await.map_err(fail)?;
        out.push(FutureRootView { code, name, venue, front, next });
    }
    Ok(Json(out))
}
