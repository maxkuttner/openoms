use uuid::Uuid;
use chrono::Utc;
use sqlx::{query_scalar, Row};
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
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderStatus, OrderType, TimeInForce};
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



pub async fn orders_cancel(
    State(state): State<AppState>,
    Json(req): Json<commands::CancelOrder>
) -> Result<Response, ApiError>{
    info!(?req, "cancel order received");

    let pool = state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());

    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;

    // start transaction
    let mut tx = pool.begin().await.map_err(|err| ApiError {
       status: StatusCode::INTERNAL_SERVER_ERROR,
       message: format!("failed to start transaction: {:?}", err),
    })?;

    let row = sqlx::query(
        r#"
        SELECT
            order_id,
            client_order_id,
            account_id,
            instrument_id,
            side,
            order_type,
            time_in_force,
            limit_price::double precision AS limit_price,
            original_qty::double precision AS original_qty,
            leaves_qty::double precision AS leaves_qty,
            cum_qty::double precision AS cum_qty,
            avg_px::double precision AS avg_px,
            status,
            resume_to_status,
            version
        FROM order_state
        WHERE order_id = $1
        "#
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load order state: {:?}", err),
    })?;
    
    // match Optional
    let row = match row {
        Some(row) => row,
        None => {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: "order does not exist".to_string(),
            })
        }
    };

    let side = parse_order_side(row.get::<String, _>("side"))?;
    let order_type = parse_order_type(row.get::<String, _>("order_type"))?;
    let time_in_force = parse_time_in_force(row.get::<String, _>("time_in_force"))?;
    let status = parse_order_status(row.get::<String, _>("status"))?;
    let resume_to_status = row
        .get::<Option<String>, _>("resume_to_status")
        .map(parse_order_status)
        .transpose()?;

    //FIXME: there must be a smarter way to instantiate the struct with sqlx
    let state = OrderAggregateState {
        order_id: row.get::<Uuid, _>("order_id").to_string(),
        client_order_id: row.get::<String, _>("client_order_id"),
        account_id: row.get::<String, _>("account_id"),
        instrument_id: row.get::<String, _>("instrument_id"),
        side,
        order_type,
        time_in_force,
        limit_price: row.get::<Option<f64>, _>("limit_price"),
        original_qty: row.get::<f64, _>("original_qty"),
        leaves_qty: row.get::<f64, _>("leaves_qty"),
        cum_qty: row.get::<f64, _>("cum_qty"),
        avg_px: row.get::<Option<f64>, _>("avg_px"),
        status,
        resume_to_status,
        version: row.get::<i64, _>("version"),
    };

    let aggregate = OrderAggregate::from_state(state.clone());
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: "oms".to_string(),
    };

    // run through state machine and decide whether can proceed
    let events = aggregate
        .decide(OrderCommand::CancelOrder(req), metadata)
        .map_err(map_rejection_to_api_error)?;

    let expected_version = state.version;
    let mut applied = OrderAggregate::from_state(state);

    for event in &events {
        applied.apply(event).map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to apply domain event {:?}", err),
        })?;
    }


    // return a Result (i.e. the value or an error)
    let state = applied.state.ok_or(ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "missing aggregate state after cancel".to_string(),
    })?;

    let updated = sqlx::query(
        r#"
        UPDATE order_state 
        SET 
          status = $2,
          resume_to_status = $3,
          version = $4,
          updated_at = $5
         
        WHERE order_id = $1 AND version = $6
        "#
    )
    .bind(order_id)
    .bind(state.status.as_str())
    .bind(state.resume_to_status.map(|status| status.as_str()))
    .bind(state.version)
    .bind(chrono::Utc::now())
    .bind(expected_version)
    .execute(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to update order state: {:?}", err),
    })?;

    if updated.rows_affected() == 0 {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "order state version mismatch".to_string(),
        });
    }


    // add events to be stored to db in order_event
    let mut new_events = Vec::with_capacity(events.len());
    for event in &events {
        new_events.push(domain_event_to_new_event(event)?);
    }

    event_store
        .append_events_in_tx(&mut tx, order_id, expected_version, &new_events)
        .await
        .map_err(|err| {
            error!(error = ?err, order_id = %order_id, "failed to append order audit event");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to append order audit event: {:?}", err),
            }
        })?;

    tx.commit().await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to commit transaction: {:?}", err),
    })?;

    // return response 
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

fn parse_order_side(value: String) -> Result<OrderSide, ApiError> {
    match value.as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state side: {}", value),
        }),
    }
}

fn parse_order_type(value: String) -> Result<OrderType, ApiError> {
    match value.as_str() {
        "market" => Ok(OrderType::Market),
        "limit" => Ok(OrderType::Limit),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state order_type: {}", value),
        }),
    }
}

fn parse_time_in_force(value: String) -> Result<TimeInForce, ApiError> {
    match value.as_str() {
        "day" => Ok(TimeInForce::Day),
        "gtc" => Ok(TimeInForce::Gtc),
        "ioc" => Ok(TimeInForce::Ioc),
        "fok" => Ok(TimeInForce::Fok),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state time_in_force: {}", value),
        }),
    }
}

fn parse_order_status(value: String) -> Result<OrderStatus, ApiError> {
    match value.as_str() {
        "submitted" => Ok(OrderStatus::Submitted),
        "routed" => Ok(OrderStatus::Routed),
        "partially_filled" => Ok(OrderStatus::PartiallyFilled),
        "filled" => Ok(OrderStatus::Filled),
        "rejected" => Ok(OrderStatus::Rejected),
        "canceled" => Ok(OrderStatus::Canceled),
        "expired" => Ok(OrderStatus::Expired),
        "suspended" => Ok(OrderStatus::Suspended),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state status: {}", value),
        }),
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




 
