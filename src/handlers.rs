use uuid::Uuid;
use chrono::Utc;
use sqlx::query_scalar;
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
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::OrderCommand;
use crate::domain::orders::errors::{CommandRejection, RejectionCode};
use crate::domain::orders::events::OrderDomainEvent;
use crate::event_store::{OrderEventStore, NewOrderEvent};
use crate::domain::orders::aggregate;

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

    info!(?req, "submit order received");
    let pool = state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());
    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;
    
    // start of the transaction
    let mut tx = pool.begin().await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to start transaction: {:?}", err),
    })?;

    let exists: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM order_state WHERE order_id = $1)"
    )
    .bind(order_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check order state: {:?}", err),
    })?;

    if exists {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "order already exists".to_string(),
        });
    }

    // create empty aggregate
    let aggregate = OrderAggregate::empty();
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: "oms".to_string(),
    };

    // run through state machine and decide whether can proceed
    let events = aggregate
        .decide(OrderCommand::SubmitOrder(req), metadata)
        .map_err(map_rejection_to_api_error)?;


    // apply event(s) suggested by the state machine
    let mut applied = OrderAggregate::empty();
    for event in &events {
        applied.apply(event).map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to apply domain event: {:?}", err),
        })?;
    }


    // return a Result (i.e. the value or an error)
    let state = applied.state.ok_or(ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "missing aggregate state after submit".to_string(),
    })?;

    sqlx::query(
        r#"
        INSERT INTO order_state (
            order_id,
            client_order_id,
            account_id,
            instrument_id,
            side,
            order_type,
            time_in_force,
            limit_price,
            original_qty,
            leaves_qty,
            cum_qty,
            avg_px,
            status,
            resume_to_status,
            version
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
        )
        "#
    )
    .bind(order_id)
    .bind(&state.client_order_id) // untouched
    .bind(&state.account_id)      // untouched
    .bind(&state.instrument_id)   // untouched
    .bind(state.side.as_str())
    .bind(state.order_type.as_str())
    .bind(state.time_in_force.as_str())
    .bind(state.limit_price)
    .bind(state.original_qty)
    .bind(state.leaves_qty)
    .bind(state.cum_qty)
    .bind(state.avg_px)
    .bind(state.status.as_str())
    .bind(state.resume_to_status.map(|status| status.as_str()))
    .bind(state.version)
    .execute(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to insert order state: {:?}", err),
    })?;


    // add events to be stored to db in order_event
    let mut new_events = Vec::with_capacity(events.len());
    for event in &events {
        new_events.push(domain_event_to_new_event(event)?);
    }

    // log events:
    // - to recover state (later potentially)
    // - populate audit trail
    event_store
        .append_events_in_tx(&mut tx, order_id, 0, &new_events)
        .await
        .map_err(|err| {
            error!(error = ?err, order_id = %order_id, "failed to append order audit event");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to append order audit event: {:?}", err),
            }
        })?;

    // commit transaction
    tx.commit().await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to commit transaction: {:?}", err),
    })?;


    // return synchronous response
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}


// Function to issue an api error
fn map_rejection_to_api_error(rejection: CommandRejection) -> ApiError {
    let status = match rejection.code {
        RejectionCode::OrderAlreadyExists => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };

    ApiError {
        status,
        message: rejection.message,
    }
}

// Transform a domain event into a struct to be persisted on the db
fn domain_event_to_new_event(event: &OrderDomainEvent) -> Result<NewOrderEvent, ApiError> {
    let payload = serde_json::to_value(event).map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to serialize domain event: {:?}", err),
    })?;

    Ok(NewOrderEvent {
        event_id: event.event_id.clone(),
        event_type: event.event_type.as_str().to_string(),
        actor: event.actor.clone(),
        payload,
        correlation_id: None, // need to be defined 
        causation_id: None,   // to be provided by the client -> why was command sent
        schema_version: 0,    // indicate the schema version of the payload
    })
}




 
