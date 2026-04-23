use uuid::Uuid;
use chrono::Utc;
use sqlx::{query_scalar, Row};
use axum::{
    extract::Extension,
    extract::Path,
    extract::State,
    http::StatusCode,
    body::Body,
    response::{Response, IntoResponse},
    Json,
    http::Uri,
};

use tracing::{error, info, warn};
use crate::adapters::{BrokerOrderRequest, BrokerError};
use crate::app_state::AppState;
use crate::domain::orders::commands;
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{OrderCommand, RouteOrder, SubmitOrder, CancelOrder};
use crate::domain::orders::errors::{CommandRejection, RejectionCode};
use crate::domain::orders::events::OrderDomainEvent;
use crate::auth::AuthContext;
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderStatus, OrderType, TimeInForce};
use crate::event_store::{OrderEventStore, NewOrderEvent};
use crate::kafka::publish_events;

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


/*
* -----------------------------
* Missing Handlers:
* TODO: 
* - orders_replace
* - orders_suspend
* - orders_release
* - orders_expire
* - orders_execution_report 
* -----------------------------
*/


// Handler: page not found
pub async fn handler_404(uri: Uri) -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        format!("No route found for path: {}", uri),
    )
}

#[utoipa::path(
    get, path = "/health",
    responses((status = 200, description = "OK", body = String))
)]
pub async fn health() -> &'static str {
    "OK"
}


#[utoipa::path(
    post, path = "/orders/submit", tag = "orders",
    request_body = SubmitOrder,
    responses(
        (status = 204, description = "Order accepted and routed"),
        (status = 400, description = "Validation error"),
        (status = 403, description = "No trade grant for principal/book/account"),
        (status = 409, description = "Order already exists"),
        (status = 502, description = "Broker rejected the order"),
        (status = 503, description = "No broker adapter configured"),
    ),
    security(("basic_auth" = []))
)]
pub async fn orders_submit(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<commands::SubmitOrder>
) -> Result<Response, ApiError> {  

    info!(?req, principal_id = %auth.principal_id, "submit order received");
    let pool = state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());
    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;
    let book_id = Uuid::parse_str(&req.book_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "book_id must be a UUID".to_string(),
    })?;
    let account_id = Uuid::parse_str(&req.account_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "account_id must be a UUID".to_string(),
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

    let principal_id = auth.principal_id;
    info!(principal_id = %principal_id, "resolved principal");

    let has_grant: bool = query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM oms_principal_book_account_grant
            WHERE principal_id = $1
              AND book_id = $2
              AND account_id = $3
              AND can_trade = true
        )"
    )
    .bind(principal_id)
    .bind(book_id)
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check grant: {:?}", err),
    })?;

    info!(has_grant, book_id = %book_id, account_id = %account_id, "checked trade grant");

    if !has_grant {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "unauthorized".to_string(),
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


    // Clone the aggregate state so applied remains usable for the RouteOrder command in TX2.
    let state_after_submit = applied.state.clone().ok_or(ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "missing aggregate state after submit".to_string(),
    })?;

    sqlx::query(
        r#"
        INSERT INTO order_state (
            order_id,
            client_order_id,
            book_id,
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
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16
        )
        "#
    )
    .bind(order_id)
    .bind(&state_after_submit.client_order_id)
    .bind(Uuid::parse_str(&state_after_submit.book_id).unwrap())
    .bind(Uuid::parse_str(&state_after_submit.account_id).unwrap())
    .bind(&state_after_submit.instrument_id)
    .bind(state_after_submit.side.as_str())
    .bind(state_after_submit.order_type.as_str())
    .bind(state_after_submit.time_in_force.as_str())
    .bind(state_after_submit.limit_price)
    .bind(state_after_submit.original_qty)
    .bind(state_after_submit.leaves_qty)
    .bind(state_after_submit.cum_qty)
    .bind(state_after_submit.avg_px)
    .bind(state_after_submit.status.as_str())
    .bind(state_after_submit.resume_to_status.map(|status| status.as_str()))
    .bind(state_after_submit.version)
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

    // commit transaction (TX1)
    tx.commit().await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to commit transaction: {:?}", err),
    })?;

    publish_events(state.kafka(), &order_id.to_string(), &events).await;

    // --- TX2: route order to broker ---
    // Look up the account to get broker_code, environment, external_account_ref.
    let account_row = sqlx::query(
        "SELECT broker_code, environment, external_account_ref FROM oms_account WHERE id = $1"
    )
    .bind(account_id)
    .fetch_optional(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load account for routing: {:?}", err),
    })?;

    let account_row = match account_row {
        Some(row) => row,
        None => {
            warn!(account_id = %account_id, "account not found during routing — order left as Submitted");
            return Ok(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap());
        }
    };

    let broker_code: String = account_row.get("broker_code");
    let environment: String = account_row.get("environment");
    let external_account_ref: String = account_row.get("external_account_ref");

    // Find the adapter for this broker+environment combination.
    let adapter = match state.registry().get(&broker_code, &environment) {
        Some(a) => a,
        None => {
            warn!(broker_code = %broker_code, environment = %environment, "no adapter registered — order left as Submitted");
            return Err(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("no adapter configured for {broker_code}/{environment}"),
            });
        }
    };

    // Call the broker adapter.
    // instrument_id is used directly as the broker symbol (TODO: instrument master table).
    let broker_req = BrokerOrderRequest {
        order_id: order_id.to_string(),
        symbol: state_after_submit.instrument_id.clone(),
        quantity: state_after_submit.original_qty,
        side: state_after_submit.side.as_str().to_string(),
        order_type: state_after_submit.order_type.as_str().to_string(),
        time_in_force: state_after_submit.time_in_force.as_str().to_string(),
        limit_price: state_after_submit.limit_price,
        external_account_ref,
    };

    let broker_resp = match adapter.submit_order(&broker_req).await {
        Ok(resp) => resp,
        Err(BrokerError::BrokerRejected(msg)) => {
            warn!(broker_code = %broker_code, error = %msg, "broker rejected order — order left as Submitted");
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!("broker rejected order: {msg}"),
            });
        }
        Err(err) => {
            error!(broker_code = %broker_code, error = %err, "adapter error routing order — order left as Submitted");
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!("failed to route order to {broker_code}: {err}"),
            });
        }
    };

    info!(
        order_id = %order_id,
        external_order_id = %broker_resp.external_order_id,
        broker = %broker_code,
        "order routed to broker"
    );

    // Persist the OrderRouted event in TX2.
    let route_metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: "oms".to_string(),
    };

    let route_events = applied
        .decide(
            OrderCommand::RouteOrder(RouteOrder {
                order_id: order_id.to_string(),
                venue: broker_code.clone(),
                external_order_id: broker_resp.external_order_id.clone(),
            }),
            route_metadata,
        )
        .map_err(map_rejection_to_api_error)?;

    for event in &route_events {
        applied.apply(event).map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to apply RouteOrder event: {:?}", err),
        })?;
    }

    let routed_state = applied.state.as_ref().ok_or(ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "missing aggregate state after routing".to_string(),
    })?;

    // TODO: outbox pattern — if TX2 fails, the broker holds the order but OMS records Submitted.
    // Detect and reconcile via a stale-Submitted sweep job.
    let mut tx2 = pool.begin().await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to start TX2: {:?}", err),
    })?;

    sqlx::query(
        r#"
        UPDATE order_state
        SET status = $2, version = $3, updated_at = $4
        WHERE order_id = $1 AND version = $5
        "#
    )
    .bind(order_id)
    .bind(routed_state.status.as_str())
    .bind(routed_state.version)
    .bind(Utc::now())
    .bind(state_after_submit.version)
    .execute(&mut *tx2)
    .await
    .map_err(|err| {
        error!(order_id = %order_id, error = ?err, "TX2: failed to update order_state to Routed");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to update order_state after routing: {:?}", err),
        }
    })?;

    let mut route_new_events = Vec::with_capacity(route_events.len());
    for event in &route_events {
        route_new_events.push(domain_event_to_new_event(event)?);
    }

    event_store
        .append_events_in_tx(&mut tx2, order_id, state_after_submit.version, &route_new_events)
        .await
        .map_err(|err| {
            error!(order_id = %order_id, error = ?err, "TX2: failed to append OrderRouted event");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to persist OrderRouted event: {:?}", err),
            }
        })?;

    tx2.commit().await.map_err(|err| {
        error!(order_id = %order_id, error = ?err, "TX2 commit failed — broker holds order but OMS status is Submitted");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to commit routing transaction: {:?}", err),
        }
    })?;

    publish_events(state.kafka(), &order_id.to_string(), &route_events).await;

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}



#[utoipa::path(
    post, path = "/orders/cancel", tag = "orders",
    request_body = CancelOrder,
    responses(
        (status = 204, description = "Cancel accepted"),
        (status = 400, description = "Invalid UUID"),
        (status = 404, description = "Order not found"),
        (status = 409, description = "Order state version mismatch"),
    ),
    security(("basic_auth" = []))
)]
pub async fn orders_cancel(
    State(app_state): State<AppState>,
    Json(req): Json<commands::CancelOrder>
) -> Result<Response, ApiError>{
    info!(?req, "cancel order received");

    let pool = app_state.pool().clone();
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
            book_id,
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
        book_id: row.get::<Uuid, _>("book_id").to_string(),
        account_id: row.get::<Uuid, _>("account_id").to_string(),
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

    publish_events(app_state.kafka(), &order_id.to_string(), &events).await;

    // return response
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}


#[utoipa::path(
    get, path = "/orders/{id}", tag = "orders",
    params(("id" = Uuid, Path, description = "Order ID")),
    responses(
        (status = 200, description = "OK", body = OrderAggregateState),
        (status = 400, description = "Invalid UUID"),
        (status = 404, description = "Order not found"),
    ),
    security(("basic_auth" = []))
)]
pub async fn get_order(
    State(state): State<AppState>,
    Path(order_id_str): Path<String>,
) -> Result<Json<OrderAggregateState>, ApiError> {
    let order_id = Uuid::parse_str(&order_id_str).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "id must be a UUID".to_string(),
    })?;

    let row = sqlx::query(
        r#"
        SELECT
            order_id, client_order_id, book_id, account_id, instrument_id,
            side, order_type, time_in_force,
            limit_price::double precision AS limit_price,
            original_qty::double precision AS original_qty,
            leaves_qty::double precision AS leaves_qty,
            cum_qty::double precision AS cum_qty,
            avg_px::double precision AS avg_px,
            status, resume_to_status, version
        FROM order_state
        WHERE order_id = $1
        "#,
    )
    .bind(order_id)
    .fetch_optional(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load order: {:?}", err),
    })?
    .ok_or_else(|| ApiError {
        status: StatusCode::NOT_FOUND,
        message: "order not found".to_string(),
    })?;

    let order = OrderAggregateState {
        order_id: row.get::<Uuid, _>("order_id").to_string(),
        client_order_id: row.get("client_order_id"),
        book_id: row.get::<Uuid, _>("book_id").to_string(),
        account_id: row.get::<Uuid, _>("account_id").to_string(),
        instrument_id: row.get("instrument_id"),
        side: parse_order_side(row.get("side"))?,
        order_type: parse_order_type(row.get("order_type"))?,
        time_in_force: parse_time_in_force(row.get("time_in_force"))?,
        limit_price: row.get("limit_price"),
        original_qty: row.get("original_qty"),
        leaves_qty: row.get("leaves_qty"),
        cum_qty: row.get("cum_qty"),
        avg_px: row.get("avg_px"),
        status: parse_order_status(row.get("status"))?,
        resume_to_status: row
            .get::<Option<String>, _>("resume_to_status")
            .map(parse_order_status)
            .transpose()?,
        version: row.get("version"),
    };

    Ok(Json(order))
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

pub(crate) fn parse_order_side(value: String) -> Result<OrderSide, ApiError> {
    match value.as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state side: {}", value),
        }),
    }
}

pub(crate) fn parse_order_type(value: String) -> Result<OrderType, ApiError> {
    match value.as_str() {
        "market" => Ok(OrderType::Market),
        "limit" => Ok(OrderType::Limit),
        _ => Err(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("invalid order_state order_type: {}", value),
        }),
    }
}


pub(crate) fn parse_time_in_force(value: String) -> Result<TimeInForce, ApiError> {
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


pub(crate) fn parse_order_status(value: String) -> Result<OrderStatus, ApiError> {
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




 
