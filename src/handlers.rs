use uuid::Uuid;
use axum::{
    extract::State,
    http::StatusCode,
    body::Body,
    response::{Response, IntoResponse},
    Json,
    http::Uri, 
};

use tracing::{error, info};
use crate::app_state::AppState;
use crate::domain::orders::commands;
use crate::event_store::{OrderEventStore, NewOrderEvent};


// Generic api error struct
pub struct ApiError {
    
    pub status: StatusCode,
    pub message: String,

}

// Trait: to provide an error message
impl IntoResponse for ApiError {

    fn into_response(self) -> Response{
        return (self.status, self.message).into_response();
    }
}


// Handler: page not found
pub async fn handler_404(uri: Uri) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        format!("No route found for path: {}", uri),
    )
}

// Handler: healthcheck
pub async fn health() -> &'static str {
    "OK"
}


// Handler: orders/submit
pub async fn orders_submit(
    State(state): State<AppState>,
    Json(req): Json<commands::SubmitOrder>
) -> Result<Response, ApiError> {  

    // call service layer
    //println!("{}", req);
    info!(?req, "submit order received");
    let pool = state.pool().clone(); 

    let event_store = OrderEventStore::new(pool);
    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;

    let order_event = NewOrderEvent {
        event_id: Uuid::new_v4().to_string(),
        event_type: "order_submitted".to_string(),
        actor: "oms".to_string(),
        payload: serde_json::to_value(&req).unwrap_or(serde_json::Value::Null),
        correlation_id: None,
        causation_id: None,
        schema_version: 1,
    };

    event_store
        .append_events(order_id, 0, &[order_event])
        .await
        .map_err(|err| {
            error!(error = ?err, order_id = %order_id, "failed to append order event");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to append order event: {:?}", err),
            }
        })?;
    return Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap());
}




 
