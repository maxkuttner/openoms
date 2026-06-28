use uuid::Uuid;
use chrono::{DateTime, Utc};
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

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use crate::adapters::{BrokerOrderRequest, BrokerError};
use crate::app_state::AppState;
use crate::domain::orders::commands;
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{OrderCommand, RouteOrder, SubmitOrder, CancelOrder};
use crate::domain::orders::errors::{CommandRejection, RejectionCode};
use crate::domain::orders::events::OrderDomainEvent;
use crate::auth::AuthContext;
use crate::positions::Position;
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderStatus, OrderType, TimeInForce};
use crate::event_store::{OrderEventStore, NewOrderEvent};
use crate::kafka::publish_events;
use crate::risk_engine::{PgRiskDataProvider, RiskCheckError, RiskEngine};



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



/// Wire payload for POST /orders/submit. `account_id` is optional — when omitted it is
/// resolved from the portfolio's `default_account_id`; when present it overrides.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SubmitOrderRequest {
    pub order_id: String,
    pub client_order_id: String,
    pub portfolio_id: String,
    pub account_id: Option<String>,
    pub instrument_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub limit_price: Option<f64>,
    pub quantity: f64,
}

impl SubmitOrderRequest {
    fn into_command(self, account_id: String) -> SubmitOrder {
        SubmitOrder {
            order_id: self.order_id,
            client_order_id: self.client_order_id,
            portfolio_id: self.portfolio_id,
            account_id,
            instrument_id: self.instrument_id,
            side: self.side,
            order_type: self.order_type,
            time_in_force: self.time_in_force,
            limit_price: self.limit_price,
            quantity: self.quantity,
        }
    }
}

#[utoipa::path(
    post, path = "/orders/submit", tag = "orders",
    request_body = SubmitOrderRequest,
    responses(
        (status = 204, description = "Order accepted and routed"),
        (status = 400, description = "Validation error"),
        (status = 403, description = "No trade grant for principal/portfolio/account"),
        (status = 409, description = "Order already exists"),
        (status = 422, description = "Instrument not found, inactive, no tradeable broker mapping, or rejected by pre-trade risk"),
        (status = 502, description = "Broker rejected the order"),
        (status = 503, description = "No broker adapter configured"),
    ),
    security(("basic_auth" = []))
)]
pub async fn orders_submit(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<SubmitOrderRequest>
) -> Result<Response, ApiError> {

    info!(?req, principal_id = %auth.principal_id, "submit order received");
    let pool = state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());
    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;
    let portfolio_id = Uuid::parse_str(&req.portfolio_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "portfolio_id must be a UUID".to_string(),
    })?;
    // Resolve the account: explicit override if given, else the portfolio's default route.
    let account_id: Uuid = match req.account_id.as_deref() {
        Some(a) => Uuid::parse_str(a).map_err(|_| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "account_id must be a UUID".to_string(),
        })?,
        None => query_scalar::<_, Option<Uuid>>(
            "SELECT default_account_id FROM portfolio WHERE id = $1",
        )
        .bind(portfolio_id)
        .fetch_optional(&pool)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to load portfolio default account: {:?}", err),
        })?
        .flatten()
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "no account_id given and portfolio has no default account".to_string(),
        })?,
    };
    // instrument.id is a BIGINT surrogate key (the mdm master instrument), not a UUID.
    let instrument_id_bigint: i64 = req.instrument_id.parse().map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "instrument_id must be a BIGINT".to_string(),
    })?;

    // Boundary → domain: fold the resolved account into the pure SubmitOrder command.
    let cmd = req.into_command(account_id.to_string());

    // Pre-flight: resolve the account's routing coordinates from its broker_connection
    // (broker_code, environment) + the custodial ref, so we can validate the instrument
    // mapping before committing anything to the event store. Requires an ACTIVE connection.
    let account_row_pre = sqlx::query(
        "SELECT bc.broker_code, bc.environment, bc.code AS broker_connection_code, a.external_account_ref \
         FROM account a \
         JOIN broker_connection bc ON bc.code = a.broker_connection_code \
         WHERE a.id = $1 AND bc.status = 'ACTIVE'"
    )
    .bind(account_id)
    .fetch_optional(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load account: {:?}", err),
    })?
    .ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "account not found or its broker connection is not active".to_string(),
    })?;

    let broker_code: String = account_row_pre.get("broker_code");
    let environment: String = account_row_pre.get("environment");
    let broker_connection_code: String = account_row_pre.get("broker_connection_code");
    let external_account_ref: String = account_row_pre.get("external_account_ref");

    // Validate instrument is ACTIVE.
    let instrument_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instrument WHERE id = $1 AND status = 'ACTIVE')"
    )
    .bind(instrument_id_bigint)
    .fetch_one(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to validate instrument: {:?}", err),
    })?;

    if !instrument_ok {
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "instrument not found or not active".to_string(),
        });
    }

    // Validate broker mapping exists and is tradeable; retrieve broker-specific symbol.
    let broker_instrument_row = sqlx::query(
        "SELECT broker_symbol, native_id, min_quantity::float8 AS min_quantity \
         FROM broker_instrument \
         WHERE instrument_id = $1 AND broker_code = $2 AND is_tradeable = true"
    )
    .bind(instrument_id_bigint)
    .bind(&broker_code)
    .fetch_optional(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to validate broker instrument mapping: {:?}", err),
    })?
    .ok_or_else(|| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: format!("no tradeable mapping for instrument on broker {broker_code}"),
    })?;

    let broker_symbol: String = broker_instrument_row.get("broker_symbol");
    let broker_native_id: Option<String> = broker_instrument_row.get("native_id");

    // Broker-intrinsic floor only: reject below the broker's minimum order size
    // (synced from the broker, e.g. Alpaca min_order_size; NULL = no minimum).
    // All *admin* caps (max order/position quantity & notional) are policy, not
    // broker facts — they live in risk_limits and are enforced by the risk engine
    // (check_submit, below), keyed per (portfolio, account, instrument).
    let min_quantity: Option<f64> = broker_instrument_row.get("min_quantity");
    if let Some(min) = min_quantity {
        if cmd.quantity < min {
            return Err(ApiError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: format!("quantity {} below broker minimum {min}", cmd.quantity),
            });
        }
    }

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
            SELECT 1 FROM principal_portfolio_grant
            WHERE principal_id = $1
              AND portfolio_id = $2
              AND can_trade = true
        )"
    )
    .bind(principal_id)
    .bind(portfolio_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check grant: {:?}", err),
    })?;

    info!(has_grant, portfolio_id = %portfolio_id, account_id = %account_id, "checked trade grant");

    if !has_grant {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "unauthorized".to_string(),
        });
    }

    // Pre-trade risk check. Runs inside TX1 so the FOR UPDATE lock on the
    // risk_limits row serializes concurrent submits for the same
    // portfolio/instrument scope until this transaction commits.
    RiskEngine::new(PgRiskDataProvider::new(&mut *tx))
        .check_submit(portfolio_id, &cmd)
        .await
        .map_err(|err| match err {
            RiskCheckError::Rejected(rejection) => {
                warn!(
                    order_id = %order_id,
                    code = %rejection.code,
                    message = %rejection.message,
                    "order rejected by pre-trade risk"
                );
                // TODO: also persist an OrderDenied audit event for rejected orders.
                ApiError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    message: format!("risk check failed [{}]: {}", rejection.code, rejection.message),
                }
            }
            RiskCheckError::Data(err) => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to evaluate pre-trade risk: {:?}", err),
            },
        })?;

    // create empty aggregate
    let aggregate = OrderAggregate::empty();
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().into(),
        timestamp: Utc::now(),
        actor: "oms".into(),
    };

    // run through state machine and decide whether can proceed
    let events = aggregate
        .decide(OrderCommand::SubmitOrder(cmd), metadata)
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
            portfolio_id,
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
            version,
            broker_connection_code
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17
        )
        "#
    )
    .bind(order_id)
    .bind(&state_after_submit.client_order_id)
    .bind(Uuid::parse_str(&state_after_submit.portfolio_id).unwrap())
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
    .bind(&broker_connection_code)
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
    // broker_code, environment, external_account_ref, and broker_symbol were resolved in the
    // pre-flight validation block before TX1.

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
    let broker_req = BrokerOrderRequest {
        order_id: order_id.to_string(),
        symbol: broker_symbol.clone(),
        native_id: broker_native_id,
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
            portfolio_id,
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
        portfolio_id: row.get::<Uuid, _>("portfolio_id").to_string(),
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
            order_id, client_order_id, portfolio_id, account_id, instrument_id,
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
        portfolio_id: row.get::<Uuid, _>("portfolio_id").to_string(),
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

#[utoipa::path(
    get, path = "/portfolios/{id}/positions", tag = "orders",
    params(("id" = Uuid, Path, description = "Portfolio ID")),
    responses(
        (status = 200, description = "OK", body = [Position]),
        (status = 403, description = "No view grant for principal/portfolio"),
    ),
    security(("basic_auth" = []))
)]
pub async fn get_portfolio_positions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(portfolio_id): Path<Uuid>,
) -> Result<Json<Vec<Position>>, ApiError> {
    let can_view: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal_portfolio_grant \
         WHERE principal_id = $1 AND portfolio_id = $2 AND can_view = true)",
    )
    .bind(auth.principal_id)
    .bind(portfolio_id)
    .fetch_one(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check grant: {:?}", err),
    })?;
    if !can_view {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "unauthorized".to_string(),
        });
    }

    let rows = sqlx::query(
        "SELECT portfolio_id, instrument_id, \
                net_qty::double precision      AS net_qty, \
                avg_cost::double precision     AS avg_cost, \
                realized_pnl::double precision AS realized_pnl, \
                updated_at \
         FROM position WHERE portfolio_id = $1 ORDER BY instrument_id",
    )
    .bind(portfolio_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load positions: {:?}", err),
    })?;

    let positions = rows
        .iter()
        .map(|r| Position {
            portfolio_id: r.get("portfolio_id"),
            instrument_id: r.get("instrument_id"),
            net_qty: r.get("net_qty"),
            avg_cost: r.get("avg_cost"),
            realized_pnl: r.get("realized_pnl"),
            updated_at: r.get("updated_at"),
        })
        .collect();
    Ok(Json(positions))
}

// ── Post-trade allocation ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AllocationSplit {
    pub portfolio_id: Uuid,
    pub qty: f64,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateAllocations {
    pub splits: Vec<AllocationSplit>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Allocation {
    pub id: Uuid,
    pub order_id: Uuid,
    pub from_portfolio_id: Uuid,
    pub to_portfolio_id: Uuid,
    pub instrument_id: String,
    pub qty: f64,
    pub price: f64,
    pub created_at: DateTime<Utc>,
}

#[utoipa::path(
    post, path = "/orders/{id}/allocations", tag = "orders",
    params(("id" = Uuid, Path, description = "Block order ID")),
    request_body = CreateAllocations,
    responses(
        (status = 200, description = "Allocated", body = [Allocation]),
        (status = 403, description = "No allocate grant on the block portfolio"),
        (status = 404, description = "Order not found"),
        (status = 422, description = "Nothing filled, over-allocation, or invalid target"),
    ),
    security(("basic_auth" = []))
)]
pub async fn create_allocations(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(order_id): Path<Uuid>,
    Json(req): Json<CreateAllocations>,
) -> Result<Json<Vec<Allocation>>, ApiError> {
    let pool = state.pool();
    let err500 = |m: String| ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: m };
    let err422 = |m: &str| ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, message: m.to_string() };

    // 1. the block order: source portfolio, instrument, side, filled qty + avg price.
    let order = sqlx::query(
        "SELECT portfolio_id, instrument_id, side, \
                cum_qty::double precision AS cum_qty, \
                avg_px::double precision  AS avg_px \
         FROM order_state WHERE order_id = $1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| err500(format!("failed to load order: {e:?}")))?
    .ok_or_else(|| ApiError { status: StatusCode::NOT_FOUND, message: "order not found".to_string() })?;

    let from_portfolio: Uuid = order.get("portfolio_id");
    let instrument_id: String = order.get("instrument_id");
    let side = parse_order_side(order.get("side"))?;
    let cum_qty: f64 = order.get("cum_qty");
    let avg_px: Option<f64> = order.get("avg_px");
    if cum_qty <= 0.0 {
        return Err(err422("order has no filled quantity to allocate"));
    }
    let price = avg_px.ok_or_else(|| err422("order has no fill price"))?;

    // 2. entitlement: can_allocate on the block (source) portfolio.
    let can_allocate: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal_portfolio_grant \
         WHERE principal_id = $1 AND portfolio_id = $2 AND can_allocate = true)",
    )
    .bind(auth.principal_id)
    .bind(from_portfolio)
    .fetch_one(pool)
    .await
    .map_err(|e| err500(format!("failed to check grant: {e:?}")))?;
    if !can_allocate {
        return Err(ApiError { status: StatusCode::FORBIDDEN, message: "unauthorized".to_string() });
    }

    // 3. validate splits + cap at the filled (and not-yet-allocated) quantity.
    if req.splits.is_empty() {
        return Err(err422("no splits provided"));
    }
    if req.splits.iter().any(|s| s.qty <= 0.0) {
        return Err(err422("split qty must be > 0"));
    }
    let total_new: f64 = req.splits.iter().map(|s| s.qty).sum();
    let already: f64 = query_scalar(
        "SELECT COALESCE(SUM(qty), 0)::double precision FROM allocation WHERE order_id = $1",
    )
    .bind(order_id)
    .fetch_one(pool)
    .await
    .map_err(|e| err500(format!("failed to sum allocations: {e:?}")))?;
    if already + total_new > cum_qty + 1e-9 {
        return Err(err422("over-allocation: would exceed the filled quantity"));
    }
    // every target portfolio must exist.
    let mut target_ids: Vec<Uuid> = req.splits.iter().map(|s| s.portfolio_id).collect();
    target_ids.sort();
    target_ids.dedup();
    let existing: i64 = query_scalar("SELECT count(*) FROM portfolio WHERE id = ANY($1)")
        .bind(&target_ids)
        .fetch_one(pool)
        .await
        .map_err(|e| err500(format!("failed to check portfolios: {e:?}")))?;
    if existing != target_ids.len() as i64 {
        return Err(err422("a target portfolio does not exist"));
    }

    // 4. apply atomically: record each allocation + transfer the position at cost.
    let mut tx = pool.begin().await.map_err(|e| err500(format!("tx begin: {e:?}")))?;
    let mut created = Vec::with_capacity(req.splits.len());
    for s in &req.splits {
        let row = sqlx::query(
            "INSERT INTO allocation \
                (order_id, from_portfolio_id, to_portfolio_id, instrument_id, qty, price) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, created_at",
        )
        .bind(order_id)
        .bind(from_portfolio)
        .bind(s.portfolio_id)
        .bind(&instrument_id)
        .bind(s.qty)
        .bind(price)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| err500(format!("failed to insert allocation: {e:?}")))?;

        crate::positions::persist_transfer(
            &mut *tx,
            from_portfolio,
            s.portfolio_id,
            &instrument_id,
            side,
            s.qty,
            price,
        )
        .await
        .map_err(|e| err500(format!("failed to transfer position: {e:?}")))?;

        created.push(Allocation {
            id: row.get("id"),
            order_id,
            from_portfolio_id: from_portfolio,
            to_portfolio_id: s.portfolio_id,
            instrument_id: instrument_id.clone(),
            qty: s.qty,
            price,
            created_at: row.get("created_at"),
        });
    }
    tx.commit().await.map_err(|e| err500(format!("commit: {e:?}")))?;
    Ok(Json(created))
}

#[utoipa::path(
    get, path = "/orders/{id}/allocations", tag = "orders",
    params(("id" = Uuid, Path, description = "Order ID")),
    responses((status = 200, description = "OK", body = [Allocation])),
    security(("basic_auth" = []))
)]
pub async fn list_allocations(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<Vec<Allocation>>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, order_id, from_portfolio_id, to_portfolio_id, instrument_id, \
                qty::double precision AS qty, price::double precision AS price, created_at \
         FROM allocation WHERE order_id = $1 ORDER BY created_at",
    )
    .bind(order_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load allocations: {:?}", err),
    })?;

    let allocs = rows
        .iter()
        .map(|r| Allocation {
            id: r.get("id"),
            order_id: r.get("order_id"),
            from_portfolio_id: r.get("from_portfolio_id"),
            to_portfolio_id: r.get("to_portfolio_id"),
            instrument_id: r.get("instrument_id"),
            qty: r.get("qty"),
            price: r.get("price"),
            created_at: r.get("created_at"),
        })
        .collect();
    Ok(Json(allocs))
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




 
