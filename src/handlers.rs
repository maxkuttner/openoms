use uuid::Uuid;
use chrono::{DateTime, Utc};
use sqlx::{query_scalar, PgPool, Postgres, QueryBuilder, Row};
use axum::{
    extract::Extension,
    extract::Path,
    extract::Query,
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
use crate::domain::orders::commands::{OrderCommand, RouteOrder, SubmitOrder};
use crate::domain::orders::errors::{CommandRejection, RejectionCode};
use crate::domain::orders::events::OrderDomainEvent;
use crate::auth::AuthContext;
use crate::positions::{value as value_position, Position};
use crate::domain::orders::state::{OrderAggregateState, OrderSide, OrderStatus, OrderType, TimeInForce};
use crate::event_store::{OrderEventStore, NewOrderEvent};
use crate::kafka::publish_events;
use crate::risk_engine::{PgRiskDataProvider, RiskCheckError, RiskEngine};

/// Who to record as having caused an event.
///
/// `"oms"` is reserved for events the system generates on its own — expiry
/// sweeps, reconciliation. A command that arrived with a credential is
/// attributed to that credential's principal, whether it came from a browser
/// session or an API key.
fn actor_for(auth: &AuthContext) -> String {
    auth.principal_code.clone()
}

// Generic api error struct
#[derive(Debug)]
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
    /// Client-generated UUID; also the idempotency key (a repeat is a 409).
    #[schema(example = "3f6b1c2e-8a4d-4e5f-9b21-1c2d3e4f5a6b")]
    pub order_id: String,
    /// Your own reference string, echoed back on updates.
    #[schema(example = "my-ref-001")]
    pub client_order_id: String,
    /// Portfolio UUID the order books against.
    #[schema(example = "b2c3d4e5-6f70-4812-93a4-556677889900")]
    pub portfolio_id: String,
    /// Optional account UUID; omit to use the portfolio's default account.
    #[schema(example = json!(null))]
    pub account_id: Option<String>,
    /// Instrument surrogate key (BIGINT as string), not a UUID. Omit when naming the
    /// instrument by `symbol` instead.
    #[schema(example = json!(null))]
    pub instrument_id: Option<String>,
    /// The instrument's venue-native symbol, as an alternative to `instrument_id`.
    /// Accepts the `Symbol@Venue` shorthand (`SPY260918C00770000@OPRA`) or a bare
    /// symbol qualified by the `venue` field.
    #[schema(example = "SPY260918C00770000@OPRA")]
    pub symbol: Option<String>,
    /// Venue (MIC) qualifying `symbol`. Omit when `symbol` already carries `@VENUE`,
    /// or when the symbol is unique across venues.
    #[schema(example = json!(null))]
    pub venue: Option<String>,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    /// Required for `limit` orders; omit for `market`.
    #[schema(example = json!(null))]
    pub limit_price: Option<f64>,
    #[schema(example = 1.0)]
    pub quantity: f64,
}

impl SubmitOrderRequest {
    /// `instrument_id` is the already-resolved surrogate key: the request may have
    /// named the instrument by symbol, so the caller resolves first and the command
    /// only ever carries the id.
    fn into_command(self, account_id: String, instrument_id: i64) -> SubmitOrder {
        SubmitOrder {
            order_id: self.order_id,
            client_order_id: self.client_order_id,
            portfolio_id: self.portfolio_id,
            account_id,
            instrument_id: instrument_id.to_string(),
            side: self.side,
            order_type: self.order_type,
            time_in_force: self.time_in_force,
            limit_price: self.limit_price,
            quantity: self.quantity,
        }
    }
}

/// Which entitlement a route demands on the portfolio an order books against.
#[derive(Clone, Copy)]
pub(crate) enum OrderPermission {
    View,
    Trade,
}

impl OrderPermission {
    /// The grant column. A fixed `&'static str` per variant — never caller input —
    /// so interpolating it into SQL cannot carry anything injectable.
    fn column(self) -> &'static str {
        match self {
            Self::View => "can_view",
            Self::Trade => "can_trade",
        }
    }
}

/// Assert this principal may act on the given order, by way of its portfolio.
///
/// Orders are reachable by UUID alone, so without this any valid trading token could
/// read or cancel any order in the system, including another principal's. The grant
/// lives on the portfolio, matching `get_portfolio_positions`.
///
/// A missing order is 404 and an unentitled one is 403 — deliberately distinguished.
/// Collapsing both to 404 would hide typos behind "not found"; order ids are UUIDs, so
/// confirming one exists tells an attacker who already guessed it nothing new.
pub(crate) async fn require_order_grant(
    pool: &PgPool,
    principal_id: Uuid,
    order_id: Uuid,
    permission: OrderPermission,
) -> Result<(), ApiError> {
    let granted: Option<bool> = query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM principal_portfolio_grant g \
          WHERE g.portfolio_id = os.portfolio_id AND g.principal_id = $2 \
            AND g.{} = true) \
         FROM order_state os WHERE os.order_id = $1",
        permission.column()
    ))
    .bind(order_id)
    .bind(principal_id)
    .fetch_optional(pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check grant: {:?}", err),
    })?;

    match granted {
        Some(true) => Ok(()),
        Some(false) => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "unauthorized".to_string(),
        }),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "order not found".to_string(),
        }),
    }
}

/// Split the `Symbol@Venue` shorthand into its parts. `None` when there is no `@`.
///
/// `Symbol@Venue` is already this codebase's name for instrument identity (see
/// `adapters::BrokerInstrument` and migration 0017); this only makes it addressable
/// on the wire. Splits on the last `@` so a symbol containing one is still reachable
/// by qualifying it — no venue code contains `@`.
fn split_symbol_at_venue(symbol: &str) -> Option<(&str, &str)> {
    let (sym, venue) = symbol.rsplit_once('@')?;
    (!sym.is_empty() && !venue.is_empty()).then_some((sym, venue))
}

/// Resolve whichever instrument reference the request carried into the surrogate key.
///
/// Three accepted forms — `instrument_id`, `symbol` + `venue`, or `symbol` as
/// `Symbol@Venue` — because `instrument` is uniquely keyed on `(symbol, venue)`, so a
/// symbol plus its venue is as precise as the id and far easier to write by hand.
///
/// Only ACTIVE rows resolve, matching `symbology_resolver`. That means an expired
/// contract fails here with `instrument not found` rather than reaching the
/// `instrument not active` check below — the same refusal, one step earlier.
async fn resolve_instrument_ref(
    pool: &PgPool,
    req: &SubmitOrderRequest,
) -> Result<i64, ApiError> {
    let bad_request = |message: &str| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: message.to_string(),
    };

    // The id wins when given: it is unambiguous, and honouring it keeps every existing
    // caller working unchanged.
    if let Some(id) = req.instrument_id.as_deref().filter(|s| !s.is_empty()) {
        return id.parse().map_err(|_| bad_request("instrument_id must be a BIGINT"));
    }

    let Some(symbol) = req.symbol.as_deref().filter(|s| !s.is_empty()) else {
        return Err(bad_request("one of instrument_id or symbol is required"));
    };

    // A venue in both places is a contradiction, not something to silently pick from —
    // resolving one over the other would trade on a venue the caller did not name.
    let (symbol, venue) = match (split_symbol_at_venue(symbol), req.venue.as_deref()) {
        (Some(_), Some(v)) if !v.is_empty() => {
            return Err(bad_request(
                "venue given both in symbol (SYMBOL@VENUE) and in the venue field",
            ))
        }
        (Some((s, v)), _) => (s, Some(v)),
        (None, v) => (symbol, v.filter(|v| !v.is_empty())),
    };

    if let Some(venue) = venue {
        return query_scalar::<_, i64>(
            "SELECT id FROM instrument WHERE symbol = $1 AND venue = $2 AND status = 'ACTIVE'",
        )
        .bind(symbol)
        .bind(venue)
        .fetch_optional(pool)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to resolve instrument: {:?}", err),
        })?
        .ok_or_else(|| ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "instrument not found".to_string(),
        });
    }

    // Unqualified: usable only when the symbol names exactly one instrument. Options
    // always do (one venue, OPRA); equities often do not, because the same ticker is
    // listed under several exchange labels and each maps to a distinct MIC.
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, venue FROM instrument WHERE symbol = $1 AND status = 'ACTIVE' ORDER BY venue",
    )
    .bind(symbol)
    .fetch_all(pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to resolve instrument: {:?}", err),
    })?;

    match rows.as_slice() {
        [(id, _)] => Ok(*id),
        [] => Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "instrument not found".to_string(),
        }),
        // Name the candidates: the caller cannot guess which venues exist, and the fix
        // is mechanical once they can see them.
        many => Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!(
                "symbol {symbol} is ambiguous across venues: [{}] — qualify it as {symbol}@VENUE",
                many.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }),
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
    security(("basic_auth" = []), ("bearer_token" = []))
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
    // Captured before `req` is moved into `cmd` below; drives the ownership
    // check further down (an explicit account_id must belong to portfolio_id).
    let account_id_was_explicit = req.account_id.is_some();
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
    // The request may have named the instrument by symbol instead; either way the
    // command below carries only the id.
    let instrument_id_bigint = resolve_instrument_ref(&pool, &req).await?;

    // Boundary → domain: fold the resolved account into the pure SubmitOrder command.
    let cmd = req.into_command(account_id.to_string(), instrument_id_bigint);

    // Pre-flight: resolve the account's routing coordinates from its broker_connection
    // (broker_code, environment) + the custodial ref, so we can validate the instrument
    // mapping before committing anything to the event store. Requires an ACTIVE connection.
    let account_row_pre = sqlx::query(
        "SELECT bc.broker_code, bc.environment, bc.code AS broker_connection_code, \
                a.external_account_ref, a.portfolio_id AS account_portfolio_id \
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

    // account.portfolio_id and portfolio.default_account_id are two
    // independent, both-mutable sources of truth (admin CRUD can repoint
    // either one without touching the other) — so both paths that can land
    // here need the ownership check, not just the explicit one.
    let account_portfolio_id: Option<Uuid> = account_row_pre.get("account_portfolio_id");
    if account_id_was_explicit {
        // Explicit account_id: must belong to this portfolio. NULL (not yet
        // backfilled) does not count as belonging — an explicit pick is a
        // deliberate cross-check, so require an exact match.
        if account_portfolio_id != Some(portfolio_id) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "account does not belong to this portfolio".to_string(),
            });
        }
    } else if let Some(other_portfolio_id) = account_portfolio_id {
        // Default-account path (portfolio.default_account_id). Unlike the
        // explicit path, NULL is fine here — it means this account predates
        // the portfolio_id backfill, not that it belongs to someone else — so
        // only reject a default account that is affirmatively owned by a
        // DIFFERENT portfolio, which is the exact cross-portfolio misroute
        // this check exists to catch.
        if other_portfolio_id != portfolio_id {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "portfolio's default account belongs to a different portfolio".to_string(),
            });
        }
    }

    let broker_code: String = account_row_pre.get("broker_code");
    let environment: String = account_row_pre.get("environment");
    let broker_connection_code: String = account_row_pre.get("broker_connection_code");
    let external_account_ref: String = account_row_pre.get("external_account_ref");

    // Validate instrument is ACTIVE.
    // Fetch the instrument's status plus the metadata the risk/routing path needs:
    // `contract_size` (the notional multiplier — 100 for options, 1 otherwise) and
    // `instrument_class` (to apply option-specific order rules).
    let instrument_row = sqlx::query(
        "SELECT instrument_class, status, \
                contract_size::double precision AS contract_size \
         FROM instrument WHERE id = $1"
    )
    .bind(instrument_id_bigint)
    .fetch_optional(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to validate instrument: {:?}", err),
    })?
    .ok_or_else(|| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: "instrument not found".to_string(),
    })?;

    let instrument_status: String = instrument_row.get("status");
    if instrument_status != "ACTIVE" {
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "instrument not active".to_string(),
        });
    }
    let instrument_class: String = instrument_row.get("instrument_class");
    let contract_size: f64 = instrument_row.get("contract_size");

    // Options route to Alpaca as single-leg contract orders, which only accept
    // a `day` time-in-force. Reject anything else up front with a clear message.
    if instrument_class == "OPTION" && !matches!(cmd.time_in_force, TimeInForce::Day) {
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "option orders require time_in_force=day".to_string(),
        });
    }

    // Validate broker mapping exists and is tradeable; retrieve broker-specific symbol.
    // Resolves through broker_instrument (the broker routing mapping).
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
        .check_submit(portfolio_id, &cmd, contract_size)
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
        actor: actor_for(&auth),
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
            principal_id,
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
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18
        )
        "#
    )
    .bind(order_id)
    .bind(&state_after_submit.client_order_id)
    .bind(auth.principal_id)
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
    //
    // A fast broker fill can race this: execution.rs's process_execution_report
    // already retries on exactly this version conflict (a Binance market order
    // can fill before we even get here), but until now this side of the same
    // race just 500'd. Re-read + re-decide + retry, mirroring execution.rs's
    // apply_once, instead of trusting the in-memory `state_after_submit` from
    // before the broker round-trip.
    const MAX_ROUTE_ATTEMPTS: u32 = 5;
    let mut route_events: Vec<OrderDomainEvent> = Vec::new();
    for attempt in 1..=MAX_ROUTE_ATTEMPTS {
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
        .fetch_one(&pool)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to re-read order state before routing: {:?}", err),
        })?;

        let side = parse_order_side(row.get::<String, _>("side"))?;
        let order_type = parse_order_type(row.get::<String, _>("order_type"))?;
        let time_in_force = parse_time_in_force(row.get::<String, _>("time_in_force"))?;
        let status = parse_order_status(row.get::<String, _>("status"))?;
        let resume_to_status = row
            .get::<Option<String>, _>("resume_to_status")
            .map(parse_order_status)
            .transpose()?;

        let current_state = OrderAggregateState {
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

        let expected_version = current_state.version;
        let mut applied = OrderAggregate::from_state(current_state);

        let route_metadata = EventMetadata {
            event_id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            actor: actor_for(&auth),
        };

        let attempt_events = applied
            .decide(
                OrderCommand::RouteOrder(RouteOrder {
                    order_id: order_id.to_string(),
                    venue: broker_code.clone(),
                    external_order_id: broker_resp.external_order_id.clone(),
                }),
                route_metadata,
            )
            .map_err(map_rejection_to_api_error)?;

        for event in &attempt_events {
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
            SET status = $2, version = $3, external_order_id = $4, updated_at = $5
            WHERE order_id = $1 AND version = $6
            "#
        )
        .bind(order_id)
        .bind(routed_state.status.as_str())
        .bind(routed_state.version)
        .bind(&broker_resp.external_order_id)
        .bind(Utc::now())
        .bind(expected_version)
        .execute(&mut *tx2)
        .await
        .map_err(|err| {
            error!(order_id = %order_id, error = ?err, "TX2: failed to update order_state to Routed");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to update order_state after routing: {:?}", err),
            }
        })?;

        let mut route_new_events = Vec::with_capacity(attempt_events.len());
        for event in &attempt_events {
            route_new_events.push(domain_event_to_new_event(event)?);
        }

        match event_store
            .append_events_in_tx(&mut tx2, order_id, expected_version, &route_new_events)
            .await
        {
            Ok(_) => {}
            Err(crate::event_store::EventStoreError::Concurrency(c)) => {
                drop(tx2);
                if attempt == MAX_ROUTE_ATTEMPTS {
                    error!(order_id = %order_id, expected = c.expected, actual = c.actual, "TX2: exhausted retries appending OrderRouted event");
                    return Err(ApiError {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        message: format!(
                            "failed to persist OrderRouted event: exhausted retries on a concurrent order update (expected {}, actual {})",
                            c.expected, c.actual
                        ),
                    });
                }
                warn!(order_id = %order_id, attempt, expected = c.expected, actual = c.actual, "TX2: order_state changed concurrently (likely a fast fill), retrying route persistence");
                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
                continue;
            }
            Err(err) => {
                error!(order_id = %order_id, error = ?err, "TX2: failed to append OrderRouted event");
                return Err(ApiError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("failed to persist OrderRouted event: {:?}", err),
                });
            }
        }

        tx2.commit().await.map_err(|err| {
            error!(order_id = %order_id, error = ?err, "TX2 commit failed — broker holds order but OMS status is Submitted");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to commit routing transaction: {:?}", err),
            }
        })?;

        route_events = attempt_events;
        break;
    }

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
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn orders_cancel(
    State(app_state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<commands::CancelOrder>
) -> Result<Response, ApiError>{
    info!(?req, principal_id = %auth.principal_id, "cancel order received");

    let pool = app_state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());

    let order_id = Uuid::parse_str(&req.order_id).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "order_id must be a UUID".to_string(),
    })?;

    // Cancelling is acting on the position, so it takes can_trade rather than can_view.
    require_order_grant(&pool, auth.principal_id, order_id, OrderPermission::Trade).await?;

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
            version,
            external_order_id,
            broker_connection_code
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

    // Already done: nothing to cancel.
    let status_str: String = row.get("status");
    if matches!(status_str.as_str(), "filled" | "canceled" | "rejected" | "expired") {
        return Err(ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("order is already terminal ({status_str}); cannot cancel"),
        });
    }

    // Broker-confirmed cancel: if the order is live at a venue, request the cancel
    // there and return 202 Accepted — the execution stream finalizes OrderCanceled
    // when the broker confirms. (Fixes the fill-vs-cancel race.)
    if let Some(ext) = row.get::<Option<String>, _>("external_order_id") {
        let broker_connection_code: String = row.get("broker_connection_code");
        let conn = sqlx::query(
            "SELECT broker_code, environment FROM broker_connection WHERE code = $1",
        )
        .bind(&broker_connection_code)
        .fetch_one(&pool)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to load broker connection: {:?}", err),
        })?;
        let broker_code: String = conn.get("broker_code");
        let environment: String = conn.get("environment");

        let adapter = app_state
            .registry()
            .get(&broker_code, &environment)
            .ok_or_else(|| ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("no adapter configured for {broker_code}/{environment}"),
            })?;

        // The broker-native symbol — required by venues that scope cancels by
        // symbol (Binance). Resolve from the same broker_instrument the submit used.
        // order_state.instrument_id is TEXT (stringified bigint); the mapping key is BIGINT.
        let instrument_id_num: i64 = row.get::<String, _>("instrument_id").parse().unwrap_or_default();
        let broker_symbol: String = sqlx::query_scalar(
            "SELECT broker_symbol FROM broker_instrument \
             WHERE instrument_id = $1 AND broker_code = $2 \
             LIMIT 1",
        )
        .bind(instrument_id_num)
        .bind(&broker_code)
        .fetch_optional(&pool)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to resolve broker symbol: {:?}", err),
        })?
        .unwrap_or_default();

        adapter.cancel_order(&ext, &broker_symbol).await.map_err(|err| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("broker rejected cancel: {err}"),
        })?;

        info!(order_id = %order_id, external_order_id = %ext, "cancel requested at broker; awaiting confirmation");
        return Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap());
    }

    // No external_order_id: the order never reached a broker — cancel locally (no race).
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
        actor: actor_for(&auth),
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
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn get_order(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(order_id_str): Path<String>,
) -> Result<Json<OrderAggregateState>, ApiError> {
    let order_id = Uuid::parse_str(&order_id_str).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "id must be a UUID".to_string(),
    })?;

    require_order_grant(state.pool(), auth.principal_id, order_id, OrderPermission::View).await?;

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

/// One portfolio the caller is entitled to, with the entitlement itself.
///
/// The flags are returned rather than filtered on, so a client can grey out what it
/// cannot do instead of discovering it from a 403 mid-order.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GrantedPortfolio {
    pub portfolio_id: String,
    pub code: String,
    pub name: String,
    pub status: String,
    pub base_currency: Option<String>,
    pub can_trade: bool,
    pub can_view: bool,
    pub can_allocate: bool,
}

/// The portfolios this principal may act on.
///
/// Exists so a trading token is self-sufficient: every other trading route needs a
/// portfolio UUID, and without this the caller has to be handed one out of band (or
/// read the admin surface, which is exactly the authority a trading token must not
/// have). Scope is the principal's own grants — there is no filter to widen it.
#[utoipa::path(
    get, path = "/portfolios", tag = "orders",
    responses((status = 200, description = "OK", body = [GrantedPortfolio])),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_portfolios(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<GrantedPortfolio>>, ApiError> {
    let rows = sqlx::query(
        "SELECT p.id, p.code, p.name, p.status, p.base_currency, \
                g.can_trade, g.can_view, g.can_allocate \
         FROM principal_portfolio_grant g \
         JOIN portfolio p ON p.id = g.portfolio_id \
         WHERE g.principal_id = $1 \
         ORDER BY p.code",
    )
    .bind(auth.principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list portfolios: {:?}", err),
    })?;

    Ok(Json(
        rows.into_iter()
            .map(|r| GrantedPortfolio {
                portfolio_id: r.get::<Uuid, _>("id").to_string(),
                code: r.get("code"),
                name: r.get("name"),
                status: r.get("status"),
                base_currency: r.get("base_currency"),
                can_trade: r.get("can_trade"),
                can_view: r.get("can_view"),
                can_allocate: r.get("can_allocate"),
            })
            .collect(),
    ))
}

#[utoipa::path(
    get, path = "/portfolios/{id}/positions", tag = "orders",
    params(("id" = Uuid, Path, description = "Portfolio ID")),
    responses(
        (status = 200, description = "OK", body = [Position]),
        (status = 403, description = "No view grant for principal/portfolio"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
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

    // contract_size scales the mark into money (x100 for options, 1 for spot).
    // LEFT JOIN: a position whose master row is missing still returns, valued at 1x.
    // position.instrument_id is TEXT holding the numeric instrument.id.
    let rows = sqlx::query(
        "SELECT p.portfolio_id, p.instrument_id, \
                p.net_qty::double precision      AS net_qty, \
                p.avg_cost::double precision     AS avg_cost, \
                p.realized_pnl::double precision AS realized_pnl, \
                p.updated_at, \
                COALESCE(i.contract_size, 1)::double precision AS contract_size \
         FROM position p \
         LEFT JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.portfolio_id = $1 ORDER BY p.instrument_id",
    )
    .bind(portfolio_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load positions: {:?}", err),
    })?;

    // Look up per row rather than snapshotting the whole store: a portfolio holds a
    // handful of instruments, while the store carries every marked instrument in the
    // universe, so a clone would cost O(all marks) per request and hold the read
    // lock against the router's writes for the duration.
    let marks = state.marks();

    let positions = rows
        .iter()
        .map(|r| {
            let instrument_id: String = r.get("instrument_id");
            let net_qty: f64 = r.get("net_qty");
            let avg_cost: f64 = r.get("avg_cost");
            let contract_size: f64 = r.get("contract_size");

            let mark = instrument_id.parse::<i64>().ok().and_then(|id| marks.get(id));
            let mid = mark.map(|m| m.mid());
            let valuation = mid.map(|mid| value_position(net_qty, avg_cost, contract_size, mid));

            Position {
                portfolio_id: r.get("portfolio_id"),
                instrument_id,
                net_qty,
                avg_cost,
                realized_pnl: r.get("realized_pnl"),
                updated_at: r.get("updated_at"),
                mark: mid,
                market_value: valuation.map(|v| v.market_value),
                unrealized_pnl: valuation.map(|v| v.unrealized_pnl),
                mark_ts: mark.map(|m| m.ts),
            }
        })
        .collect();
    Ok(Json(positions))
}

/// One (broker_instrument, broker_connection) pair's usability for a
/// specific portfolio. Checked in this exact order — the response always
/// names the single most fundamental blocker, not every applicable one.
/// `connection_status` is `None` both when no broker_connection row exists
/// at all for this broker_code, and when sqlx reads a NULL from the LEFT
/// JOIN — both cases mean "nothing to route through", same as an inactive
/// connection.
fn classify_venue(
    is_tradeable: bool,
    connection_status: Option<&str>,
    has_account: bool,
) -> (bool, Option<&'static str>) {
    if !is_tradeable {
        return (false, Some("not tradeable on this broker"));
    }
    if connection_status != Some("ACTIVE") {
        return (false, Some("broker connection is not active"));
    }
    if !has_account {
        return (false, Some("no account on this connection for this portfolio"));
    }
    (true, None)
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct VenueOption {
    pub broker_code: String,
    pub environment: Option<String>,
    pub broker_connection_code: Option<String>,
    pub account_id: Option<Uuid>,
    pub eligible: bool,
    pub reason: Option<String>,
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct VenuesQuery {
    pub instrument_id: String,
}

#[utoipa::path(
    get, path = "/portfolios/{id}/venues", tag = "orders",
    params(
        ("id" = Uuid, Path, description = "Portfolio ID"),
        VenuesQuery,
    ),
    responses(
        (status = 200, description = "OK", body = [VenueOption]),
        (status = 403, description = "No view grant for principal/portfolio"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_portfolio_venues(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(portfolio_id): Path<Uuid>,
    Query(query): Query<VenuesQuery>,
) -> Result<Json<Vec<VenueOption>>, ApiError> {
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
        return Err(ApiError { status: StatusCode::FORBIDDEN, message: "unauthorized".to_string() });
    }

    // An unparseable instrument_id has no venues, same as a real but
    // never-synced one — excluded, not an error.
    let Ok(instrument_id) = query.instrument_id.parse::<i64>() else {
        return Ok(Json(Vec::new()));
    };

    let rows = sqlx::query(
        "SELECT bi.broker_code, bi.is_tradeable, bc.environment, \
                bc.code AS broker_connection_code, bc.status AS connection_status, \
                a.id AS account_id \
         FROM broker_instrument bi \
         LEFT JOIN broker_connection bc ON bc.broker_code = bi.broker_code \
         LEFT JOIN account a ON a.broker_connection_code = bc.code AND a.portfolio_id = $2 \
         WHERE bi.instrument_id = $1",
    )
    .bind(instrument_id)
    .bind(portfolio_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load venues: {:?}", err),
    })?;

    let options = rows
        .into_iter()
        .map(|row| {
            let is_tradeable: bool = row.get("is_tradeable");
            let connection_status: Option<String> = row.get("connection_status");
            let account_id: Option<Uuid> = row.get("account_id");
            let (eligible, reason) =
                classify_venue(is_tradeable, connection_status.as_deref(), account_id.is_some());
            VenueOption {
                broker_code: row.get("broker_code"),
                environment: row.get("environment"),
                broker_connection_code: row.get("broker_connection_code"),
                account_id,
                eligible,
                reason: reason.map(str::to_string),
            }
        })
        .collect();

    Ok(Json(options))
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
    security(("basic_auth" = []), ("bearer_token" = []))
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
    security(("basic_auth" = []), ("bearer_token" = []))
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

// ── Blotter / oversight ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BlotterFilter {
    pub status: Option<String>,
    pub portfolio_id: Option<Uuid>,
    pub instrument_id: Option<String>,
    pub principal_id: Option<Uuid>,
    pub broker_connection_code: Option<String>,
    pub side: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BlotterRow {
    pub order_id: Uuid,
    pub principal_id: Uuid,
    pub principal_code: String,
    pub portfolio_id: Uuid,
    pub portfolio_code: String,
    pub account_id: Uuid,
    pub broker_connection_code: String,
    pub instrument_id: String,
    pub instrument_symbol: Option<String>,
    pub instrument_name: Option<String>,
    pub side: String,
    pub order_type: String,
    pub status: String,
    pub original_qty: f64,
    pub leaves_qty: f64,
    pub cum_qty: f64,
    pub avg_px: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Who a blotter query is allowed to see.
///
/// The admin blotter sees everything and may filter by principal; a trading token
/// sees only what its principal is granted. Making that a parameter of one query —
/// rather than two similar queries — is what keeps the two views from drifting into
/// disagreeing about the same order.
enum BlotterScope {
    /// Oversight: every order, `principal_id` usable as a filter.
    All,
    /// This principal's entitled portfolios only. Not a filter — a ceiling.
    GrantedTo(Uuid),
}

async fn load_blotter(
    state: &AppState,
    f: &BlotterFilter,
    scope: BlotterScope,
) -> Result<Vec<BlotterRow>, ApiError> {
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT os.order_id, os.principal_id, p.code AS principal_code, \
                os.portfolio_id, pf.code AS portfolio_code, os.account_id, \
                os.broker_connection_code, os.instrument_id, \
                i.symbol AS instrument_symbol, i.name AS instrument_name, \
                os.side, os.order_type, os.status, \
                os.original_qty::double precision AS original_qty, \
                os.leaves_qty::double precision   AS leaves_qty, \
                os.cum_qty::double precision      AS cum_qty, \
                os.avg_px::double precision       AS avg_px, \
                os.created_at, os.updated_at \
         FROM order_state os \
         JOIN principal p  ON p.id  = os.principal_id \
         JOIN portfolio pf ON pf.id = os.portfolio_id \
         LEFT JOIN instrument i ON i.id::text = os.instrument_id \
         WHERE TRUE",
    );
    // Scope first, so it can never be widened by a filter appended after it.
    match scope {
        BlotterScope::All => {
            if let Some(v) = f.principal_id { qb.push(" AND os.principal_id = ").push_bind(v); }
        }
        // EXISTS rather than a join: an order must sit in a portfolio this principal
        // may view, and a join would duplicate rows if the grant model ever allows
        // more than one matching grant per portfolio.
        BlotterScope::GrantedTo(principal_id) => {
            qb.push(
                " AND EXISTS (SELECT 1 FROM principal_portfolio_grant g \
                   WHERE g.portfolio_id = os.portfolio_id AND g.can_view = true \
                     AND g.principal_id = ",
            )
            .push_bind(principal_id)
            .push(")");
        }
    }
    if let Some(v) = &f.status { qb.push(" AND os.status = ").push_bind(v.clone()); }
    if let Some(v) = f.portfolio_id { qb.push(" AND os.portfolio_id = ").push_bind(v); }
    if let Some(v) = &f.instrument_id { qb.push(" AND os.instrument_id = ").push_bind(v.clone()); }
    if let Some(v) = &f.broker_connection_code {
        qb.push(" AND os.broker_connection_code = ").push_bind(v.clone());
    }
    if let Some(v) = &f.side { qb.push(" AND os.side = ").push_bind(v.clone()); }
    if let Some(v) = f.since { qb.push(" AND os.created_at >= ").push_bind(v); }
    if let Some(v) = f.until { qb.push(" AND os.created_at <= ").push_bind(v); }
    qb.push(" ORDER BY os.created_at DESC");
    let limit = f.limit.unwrap_or(100).clamp(1, 500);
    let offset = f.offset.unwrap_or(0).max(0);
    qb.push(" LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);

    let rows = qb.build().fetch_all(state.pool()).await.map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load blotter: {:?}", err),
    })?;

    let out = rows
        .iter()
        .map(|r| BlotterRow {
            order_id: r.get("order_id"),
            principal_id: r.get("principal_id"),
            principal_code: r.get("principal_code"),
            portfolio_id: r.get("portfolio_id"),
            portfolio_code: r.get("portfolio_code"),
            account_id: r.get("account_id"),
            broker_connection_code: r.get("broker_connection_code"),
            instrument_id: r.get("instrument_id"),
            instrument_symbol: r.get("instrument_symbol"),
            instrument_name: r.get("instrument_name"),
            side: r.get("side"),
            order_type: r.get("order_type"),
            status: r.get("status"),
            original_qty: r.get("original_qty"),
            leaves_qty: r.get("leaves_qty"),
            cum_qty: r.get("cum_qty"),
            avg_px: r.get("avg_px"),
            created_at: r.get("created_at"),
            updated_at: r.get("updated_at"),
        })
        .collect();
    Ok(out)
}

#[utoipa::path(
    get, path = "/admin/orders", tag = "admin",
    params(BlotterFilter),
    responses((status = 200, description = "OK", body = [BlotterRow])),
    security(("bearer_token" = []))
)]
pub async fn get_orders_blotter(
    State(state): State<AppState>,
    Query(f): Query<BlotterFilter>,
) -> Result<Json<Vec<BlotterRow>>, ApiError> {
    load_blotter(&state, &f, BlotterScope::All).await.map(Json)
}

/// The caller's own blotter.
///
/// Same rows and same filters as the admin blotter, bounded to the portfolios this
/// principal may view. `principal_id` in the query string is ignored rather than
/// honoured — the authenticated identity is the only thing that decides scope, so
/// passing someone else's id does nothing.
#[utoipa::path(
    get, path = "/orders", tag = "orders",
    params(BlotterFilter),
    responses((status = 200, description = "OK", body = [BlotterRow])),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_orders(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Query(f): Query<BlotterFilter>,
) -> Result<Json<Vec<BlotterRow>>, ApiError> {
    load_blotter(&state, &f, BlotterScope::GrantedTo(auth.principal_id)).await.map(Json)
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

#[derive(serde::Serialize)]
pub struct WatchlistRow {
    pub instrument_id: String,
    pub symbol: String,
    pub venue: String,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
pub struct AddWatchlistItem {
    pub instrument_id: String,
}

pub async fn list_watchlist(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<WatchlistRow>>, ApiError> {
    // Joined against instrument so the trade screen can show "SYMBOL@VENUE"
    // instead of a bare numeric id — the id alone doesn't let a trader tell
    // rows apart. w.instrument_id is TEXT (see 0025_CREATE_WATCHLIST_ITEM_TABLE.sql,
    // no cross-schema FK), so it's cast to bigint on the join side rather than
    // casting i.id to text, so the instrument PK index is still usable.
    let rows = sqlx::query_as::<_, (String, String, String, String, chrono::DateTime<chrono::Utc>)>(
        "SELECT w.instrument_id, i.symbol, i.venue, i.name, w.created_at \
         FROM watchlist_item w JOIN instrument i ON i.id = w.instrument_id::bigint \
         WHERE w.principal_id = $1 ORDER BY w.created_at",
    )
    .bind(auth.principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to list watchlist: {err:?}"),
    })?;
    Ok(Json(
        rows.into_iter()
            .map(|(instrument_id, symbol, venue, name, created_at)| WatchlistRow {
                instrument_id,
                symbol,
                venue,
                name,
                created_at,
            })
            .collect(),
    ))
}

pub async fn add_watchlist_item(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AddWatchlistItem>,
) -> Result<StatusCode, ApiError> {
    // Parsed to i64 first so the bind compares against instrument.id natively
    // (uses the PK index) instead of casting the indexed column to text. An
    // instrument_id that doesn't even parse is the same "not found" outcome as
    // one that parses but doesn't exist.
    let Ok(parsed_id) = body.instrument_id.parse::<i64>() else {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no active instrument {}", body.instrument_id),
        });
    };

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instrument WHERE id = $1 AND status = 'ACTIVE')",
    )
    .bind(parsed_id)
    .fetch_one(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check instrument: {err:?}"),
    })?;
    if !exists {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no active instrument {}", body.instrument_id),
        });
    }

    sqlx::query(
        "INSERT INTO watchlist_item (principal_id, instrument_id) VALUES ($1, $2) \
         ON CONFLICT (principal_id, instrument_id) DO NOTHING",
    )
    .bind(auth.principal_id)
    .bind(&body.instrument_id)
    .execute(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to add watchlist item: {err:?}"),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_watchlist_item(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(instrument_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    sqlx::query("DELETE FROM watchlist_item WHERE principal_id = $1 AND instrument_id = $2")
        .bind(auth.principal_id)
        .bind(&instrument_id)
        .execute(state.pool())
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to remove watchlist item: {err:?}"),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct MarksQuery {
    pub instrument_ids: String, // comma-separated, e.g. "1,2,3"
}

#[derive(serde::Serialize)]
pub struct MarkRow {
    pub instrument_id: i64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mid: Option<f64>,
    pub prev_close: Option<f64>,
    pub pct_change: Option<f64>,
}

pub async fn get_marks(
    State(state): State<AppState>,
    Query(query): Query<MarksQuery>,
) -> Result<Json<Vec<MarkRow>>, ApiError> {
    let ids: Vec<i64> = query
        .instrument_ids
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let rows = ids
        .into_iter()
        .map(|instrument_id| {
            let mark = state.marks().get(instrument_id);
            let daily = state.daily_stats().get(instrument_id);
            let mid = mark.map(|m| m.mid());
            let pct_change = match (mid, daily) {
                (Some(mid), Some(d)) if d.prev_close != 0.0 => {
                    Some((mid - d.prev_close) / d.prev_close * 100.0)
                }
                _ => None,
            };
            MarkRow {
                instrument_id,
                bid: mark.map(|m| m.bid),
                ask: mark.map(|m| m.ask),
                mid,
                prev_close: daily.map(|d| d.prev_close),
                pct_change,
            }
        })
        .collect();
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_tradeable_wins_over_every_other_reason() {
        let (eligible, reason) = classify_venue(false, Some("ACTIVE"), true);
        assert!(!eligible);
        assert_eq!(reason, Some("not tradeable on this broker"));
    }

    #[test]
    fn missing_connection_is_the_same_as_inactive() {
        let (eligible, reason) = classify_venue(true, None, true);
        assert!(!eligible);
        assert_eq!(reason, Some("broker connection is not active"));
    }

    #[test]
    fn inactive_connection_beats_missing_account() {
        let (eligible, reason) = classify_venue(true, Some("SUSPENDED"), false);
        assert!(!eligible);
        assert_eq!(reason, Some("broker connection is not active"));
    }

    #[test]
    fn no_account_is_the_last_reason_checked() {
        let (eligible, reason) = classify_venue(true, Some("ACTIVE"), false);
        assert!(!eligible);
        assert_eq!(reason, Some("no account on this connection for this portfolio"));
    }

    #[test]
    fn everything_present_is_eligible_with_no_reason() {
        let (eligible, reason) = classify_venue(true, Some("ACTIVE"), true);
        assert!(eligible);
        assert_eq!(reason, None);
    }

    /// The case the shorthand exists for: an OSI symbol qualified by its venue.
    #[test]
    fn splits_symbol_at_venue() {
        assert_eq!(
            split_symbol_at_venue("SPY260918C00770000@OPRA"),
            Some(("SPY260918C00770000", "OPRA"))
        );
        assert_eq!(split_symbol_at_venue("AAPL@XNAS"), Some(("AAPL", "XNAS")));
    }

    /// No `@` means the whole string is the symbol — the caller either qualified it
    /// with the `venue` field or is relying on it being unique.
    #[test]
    fn unqualified_symbol_does_not_split() {
        assert_eq!(split_symbol_at_venue("SPY260918C00770000"), None);
        assert_eq!(split_symbol_at_venue("BTCUSDT"), None);
    }

    /// Splitting on the *last* `@` keeps a symbol that itself contains one reachable:
    /// no venue code contains `@`, so the trailing segment is always the venue.
    #[test]
    fn splits_on_the_last_at() {
        assert_eq!(split_symbol_at_venue("WEIRD@SYM@XNAS"), Some(("WEIRD@SYM", "XNAS")));
    }

    /// A dangling `@` names neither a symbol nor a venue. Declining here means the
    /// caller gets "one of instrument_id or symbol is required" or a not-found, rather
    /// than a lookup on an empty string.
    #[test]
    fn declines_half_empty_forms() {
        assert_eq!(split_symbol_at_venue("@OPRA"), None);
        assert_eq!(split_symbol_at_venue("SPY@"), None);
        assert_eq!(split_symbol_at_venue("@"), None);
    }

    /// The grant column is chosen by the route, never by the caller — these two fixed
    /// strings are the only things ever interpolated into the grant query.
    #[test]
    fn order_permission_maps_to_grant_column() {
        assert_eq!(OrderPermission::View.column(), "can_view");
        assert_eq!(OrderPermission::Trade.column(), "can_trade");
    }

    /// An authenticated command is attributed to whoever sent it — the whole
    /// point of carrying `principal_code` on `AuthContext`.
    #[test]
    fn an_authenticated_command_is_attributed_to_whoever_sent_it() {
        let auth = AuthContext {
            principal_id: Uuid::nil(),
            principal_code: "jane.doe".to_string(),
        };

        assert_eq!(actor_for(&auth), "jane.doe");
    }

    /// The test above pins `actor_for` in isolation, which is not the property
    /// that matters: a call site quietly reverting to the literal `"oms"` would
    /// compile and keep it green. This one submits a real order through
    /// `orders_submit` and reads `actor` back out of `order_event`.
    ///
    /// The principal code is deliberately longer than 30 characters, because
    /// `order_event.actor` was `VARCHAR(30)` — this test fails with Postgres
    /// 22001 against the pre-migration schema, which is exactly the defect it
    /// exists to catch (see 0024_ALTER_ORDER_EVENT_WIDEN_ACTOR.sql).
    ///
    /// No broker adapter is registered, so the submit returns 503 at the
    /// routing step — *after* TX1 has committed `OrderSubmitted`, which is the
    /// row being asserted on. Rows are left behind (order_event is append-only,
    /// guarded by an ON DELETE trigger), so every seeded code is suffixed with
    /// a fresh UUID the way `sessions.rs`'s Postgres tests do.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live, migrated Postgres reachable via the usual POSTGRES_* config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_submitted_order_records_the_principal_that_sent_it() {
        use crate::app_state::AppState;
        use crate::adapters::BrokerRegistry;
        use crate::stream_health::StreamHealthRegistry;
        use symbology::{Identifier, InMemoryCache, OpenFigiClient};

        let pool = test_pool().await;
        let suffix = Uuid::new_v4();

        let (principal_id, principal_code) = seed_principal(&pool, "audit-actor-call-site-test").await;
        assert!(
            principal_code.len() > 30,
            "the code must exceed the old VARCHAR(30) to catch C1, got {} chars",
            principal_code.len()
        );

        // Reference data the instrument FKs into. Taken from whatever the
        // database already holds rather than hard-coded, so the test does not
        // depend on one particular MIC or currency having been seeded.
        let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("a seeded venue");
        let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("a seeded currency");

        let instrument_id: i64 = sqlx::query_scalar(
            "INSERT INTO instrument \
                 (symbol, venue, name, asset_class, instrument_class, currency, status, \
                  price_precision, price_increment) \
             VALUES ($1, $2, 'Actor attribution test instrument', 'EQUITY', 'SPOT', $3, 'ACTIVE', 2, 0.01) \
             RETURNING id",
        )
        .bind(format!("ACTOR{}", suffix.simple()))
        .bind(&venue)
        .bind(&currency)
        .fetch_one(&pool)
        .await
        .expect("seed instrument");

        // A broker code no adapter is ever registered under, so routing fails
        // predictably at `registry().get(...)` instead of reaching a network.
        let broker_code = format!("ACTORTEST{}", suffix.simple());
        let connection_code = format!("actor-test-conn-{suffix}");
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) \
             VALUES ($1, $2, 'PAPER', 'ACTIVE')",
        )
        .bind(&connection_code)
        .bind(&broker_code)
        .execute(&pool)
        .await
        .expect("seed broker_connection");

        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) \
             VALUES ($1, $2, $3, true)",
        )
        .bind(instrument_id)
        .bind(&broker_code)
        .bind(format!("ACTOR{}", suffix.simple()))
        .execute(&pool)
        .await
        .expect("seed broker_instrument");

        let account_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) \
             VALUES ($1, $2, $3, $4, 'ACTIVE')",
        )
        .bind(account_id)
        .bind(format!("actor-test-account-{suffix}"))
        .bind(&connection_code)
        .bind(format!("EXT-{suffix}"))
        .execute(&pool)
        .await
        .expect("seed account");

        let portfolio_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status, default_account_id) \
             VALUES ($1, $2, 'Actor attribution test portfolio', 'ACTIVE', $3)",
        )
        .bind(portfolio_id)
        .bind(format!("actor-test-portfolio-{suffix}"))
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("seed portfolio");

        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade) \
             VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed grant");

        // ── the real order path ──────────────────────────────────────────────
        let (quote_tx, _quote_rx) = tokio::sync::mpsc::channel(1);
        let state = AppState::new(
            pool.clone(),
            "test-admin-token".to_string(),
            false,
            BrokerRegistry::new(), // empty: nothing can route
            None,                  // no Kafka; `publish_events` is a no-op without it
            Identifier::new(OpenFigiClient::new(None), InMemoryCache::new()),
            StreamHealthRegistry::new(),
            None,
            quote_tx,
            crate::sessions::SessionConfig {
                cookie_policy: crate::sessions::cookie_policy("localhost:3001", None),
                ttl: crate::sessions::SessionTtl::default(),
                public_base_url: None,
            },
        );

        let order_id = Uuid::new_v4();
        let request = SubmitOrderRequest {
            order_id: order_id.to_string(),
            client_order_id: format!("actor-test-{suffix}"),
            portfolio_id: portfolio_id.to_string(),
            account_id: None,
            instrument_id: Some(instrument_id.to_string()),
            symbol: None,
            venue: None,
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            limit_price: None,
            quantity: 1.0,
        };

        let auth = AuthContext {
            principal_id,
            principal_code: principal_code.clone(),
        };

        let outcome = orders_submit(State(state), Extension(auth), Json(request)).await;

        let err = outcome
            .err()
            .expect("no adapter is registered, so the routing step must fail");
        assert_eq!(
            err.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "expected the no-adapter failure, got: {}",
            err.message
        );

        // The point of the test: what the call site actually stamped.
        let actors: Vec<String> = sqlx::query_scalar(
            "SELECT actor FROM order_event WHERE order_id = $1 ORDER BY version",
        )
        .bind(order_id)
        .fetch_all(&pool)
        .await
        .expect("read back the audit trail");

        assert_eq!(
            actors,
            vec![principal_code.clone()],
            "the OrderSubmitted event must name the principal, not 'oms'"
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_account_from_another_portfolio_is_rejected() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "ownership-test").await;

        let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("a seeded venue");
        let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("a seeded currency");
        let symbol = format!("OWNERTEST{}", Uuid::new_v4().simple());
        let instrument_id: i64 = sqlx::query_scalar(
            "INSERT INTO instrument \
                 (symbol, venue, name, asset_class, instrument_class, currency, status, \
                  price_precision, price_increment) \
             VALUES ($1, $2, 'Ownership test instrument', 'EQUITY', 'SPOT', $3, 'ACTIVE', 2, 0.01) \
             RETURNING id",
        )
        .bind(&symbol)
        .bind(&venue)
        .bind(&currency)
        .fetch_one(&pool)
        .await
        .expect("seed instrument");

        let broker_code = format!("OWNERTEST{}", Uuid::new_v4().simple());
        let conn_code = format!("owner-test-conn-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')")
            .bind(&conn_code)
            .bind(&broker_code)
            .execute(&pool)
            .await
            .expect("seed broker_connection");
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, $3, true)")
            .bind(instrument_id)
            .bind(&broker_code)
            .bind(&symbol)
            .execute(&pool)
            .await
            .expect("seed broker_instrument");

        // Portfolio A owns account_a.
        let portfolio_a = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Portfolio A', 'ACTIVE')")
            .bind(portfolio_a)
            .bind(format!("owner-test-a-{portfolio_a}"))
            .execute(&pool)
            .await
            .expect("seed portfolio a");
        let account_a = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
             VALUES ($1, $2, $3, 'EXT-A', 'ACTIVE', $4)",
        )
        .bind(account_a)
        .bind(format!("owner-test-account-a-{account_a}"))
        .bind(&conn_code)
        .bind(portfolio_a)
        .execute(&pool)
        .await
        .expect("seed account a");

        // Portfolio B is the one on the request, with a grant to trade — but the
        // request names portfolio A's account.
        let portfolio_b = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Portfolio B', 'ACTIVE')")
            .bind(portfolio_b)
            .bind(format!("owner-test-b-{portfolio_b}"))
            .execute(&pool)
            .await
            .expect("seed portfolio b");
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_b)
        .execute(&pool)
        .await
        .expect("seed grant");

        let state = test_app_state(pool);
        let _ = principal_code; // unused here; kept for symmetry with other tests' destructuring

        let request = SubmitOrderRequest {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: "ownership-test-1".to_string(),
            portfolio_id: portfolio_b.to_string(),
            account_id: Some(account_a.to_string()),
            instrument_id: Some(instrument_id.to_string()),
            symbol: None,
            venue: None,
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            limit_price: None,
            quantity: 1.0,
        };

        let result = orders_submit(
            State(state),
            Extension(AuthContext { principal_id, principal_code: "irrelevant".to_string() }),
            Json(request),
        )
        .await;

        assert!(result.is_err());
        let err = result.err().unwrap();
        assert_eq!(
            err.status,
            StatusCode::BAD_REQUEST,
            "expected rejection for cross-portfolio account, got: {}",
            err.message
        );
        assert!(
            err.message.contains("does not belong to this portfolio"),
            "expected an ownership-mismatch message, got: {}",
            err.message
        );
    }

    /// Regression guard: a request with no `account_id`, whose default
    /// account has no `portfolio_id` set (NULL — the pre-backfill state most
    /// existing default accounts are in), must still take the same
    /// `default_account_id` fallback path as before Task 4/I1: the ownership
    /// check on this path (see `orders_submit`) treats NULL as "not yet
    /// backfilled, trust it" rather than rejecting. Mirrors the
    /// actor-attribution test's seeding (a portfolio with a
    /// `default_account_id`, an empty `BrokerRegistry` so nothing can
    /// actually route) — the point is that the failure here is the
    /// pre-existing "no adapter registered" routing failure, not the
    /// ownership rejection covered separately by
    /// `default_account_belonging_to_another_portfolio_is_rejected` below.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn omitting_account_id_is_completely_unaffected() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "default-account-test").await;
        let instrument_id = seed_instrument(&pool, "DEFACCT").await;

        let broker_code = format!("DEFACCT{}", Uuid::new_v4().simple());
        let conn_code = format!("defacct-test-conn-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')")
            .bind(&conn_code)
            .bind(&broker_code)
            .execute(&pool)
            .await
            .expect("seed broker_connection");
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'DEFACCT', true)")
            .bind(instrument_id)
            .bind(&broker_code)
            .execute(&pool)
            .await
            .expect("seed broker_instrument");

        let account_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) \
             VALUES ($1, $2, $3, 'EXT-DEFAULT', 'ACTIVE')",
        )
        .bind(account_id)
        .bind(format!("defacct-test-account-{account_id}"))
        .bind(&conn_code)
        .execute(&pool)
        .await
        .expect("seed account");

        let portfolio_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status, default_account_id) \
             VALUES ($1, $2, 'Default account test portfolio', 'ACTIVE', $3)",
        )
        .bind(portfolio_id)
        .bind(format!("defacct-test-portfolio-{portfolio_id}"))
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("seed portfolio");

        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed grant");

        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        let request = SubmitOrderRequest {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: "default-account-test-1".to_string(),
            portfolio_id: portfolio_id.to_string(),
            account_id: None,
            instrument_id: Some(instrument_id.to_string()),
            symbol: None,
            venue: None,
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            limit_price: None,
            quantity: 1.0,
        };

        let result = orders_submit(State(state), Extension(auth), Json(request)).await;

        let err = result.err().expect("no adapter is registered, so routing must fail");
        assert_eq!(
            err.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "no new ownership rejection should appear here — expected the pre-existing \
             no-adapter routing failure, got: {}",
            err.message
        );
        assert!(
            !err.message.contains("does not belong to this portfolio"),
            "the default_account_id fallback path must not trigger the explicit-path ownership check, got: {}",
            err.message
        );
    }

    /// I1 regression: `account.portfolio_id` and `portfolio.default_account_id`
    /// are two independent, both-mutable sources of truth — admin CRUD can
    /// repoint a portfolio's `default_account_id` at an account that actually
    /// belongs (via `account.portfolio_id`) to some OTHER portfolio, without
    /// anything enforcing they stay in sync. Before this fix, an omitted
    /// `account_id` blindly trusted `default_account_id` and never re-checked
    /// ownership, so this state let an order silently misroute across
    /// portfolios via the default path — exactly the class of bug the
    /// explicit-path check (`an_account_from_another_portfolio_is_rejected`)
    /// exists to prevent, just reachable a different way.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn default_account_belonging_to_another_portfolio_is_rejected() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "default-misroute-test").await;
        let instrument_id = seed_instrument(&pool, "DEFMISROUTE").await;

        let broker_code = format!("DEFMISROUTE{}", Uuid::new_v4().simple());
        let conn_code = format!("defmisroute-test-conn-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')")
            .bind(&conn_code)
            .bind(&broker_code)
            .execute(&pool)
            .await
            .expect("seed broker_connection");
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'DEFMISROUTE', true)")
            .bind(instrument_id)
            .bind(&broker_code)
            .execute(&pool)
            .await
            .expect("seed broker_instrument");

        // The account actually belongs to portfolio_owner...
        let portfolio_owner = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Owner portfolio', 'ACTIVE')")
            .bind(portfolio_owner)
            .bind(format!("defmisroute-owner-{portfolio_owner}"))
            .execute(&pool)
            .await
            .expect("seed owner portfolio");
        let account_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
             VALUES ($1, $2, $3, 'EXT-DEFMISROUTE', 'ACTIVE', $4)",
        )
        .bind(account_id)
        .bind(format!("defmisroute-account-{account_id}"))
        .bind(&conn_code)
        .bind(portfolio_owner)
        .execute(&pool)
        .await
        .expect("seed account");

        // ...but portfolio_requester's default_account_id was (mis)pointed at
        // it anyway — the drift this fix catches.
        let portfolio_requester = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status, default_account_id) \
             VALUES ($1, $2, 'Requester portfolio', 'ACTIVE', $3)",
        )
        .bind(portfolio_requester)
        .bind(format!("defmisroute-requester-{portfolio_requester}"))
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("seed requester portfolio");

        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_requester)
        .execute(&pool)
        .await
        .expect("seed grant");

        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        let request = SubmitOrderRequest {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: "default-misroute-test-1".to_string(),
            portfolio_id: portfolio_requester.to_string(),
            account_id: None,
            instrument_id: Some(instrument_id.to_string()),
            symbol: None,
            venue: None,
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Day,
            limit_price: None,
            quantity: 1.0,
        };

        let result = orders_submit(State(state), Extension(auth), Json(request)).await;

        let err = result.err().expect("cross-portfolio default account must be rejected");
        assert_eq!(
            err.status,
            StatusCode::BAD_REQUEST,
            "expected rejection for a default account owned by another portfolio, got: {}",
            err.message
        );
        assert!(
            err.message.contains("belongs to a different portfolio"),
            "expected an ownership-mismatch message, got: {}",
            err.message
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn watchlist_add_list_remove_round_trips() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "watch-roundtrip").await;
        let instrument_id = seed_instrument(&pool, "RT").await.to_string();
        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        add_watchlist_item(
            State(state.clone()),
            Extension(auth.clone()),
            Json(AddWatchlistItem { instrument_id: instrument_id.clone() }),
        )
        .await
        .expect("add should succeed");

        let listed = list_watchlist(State(state.clone()), Extension(auth.clone()))
            .await
            .expect("list should succeed")
            .0;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].instrument_id, instrument_id);

        remove_watchlist_item(State(state.clone()), Extension(auth.clone()), Path(instrument_id.clone()))
            .await
            .expect("remove should succeed");

        let listed = list_watchlist(State(state), Extension(auth)).await.expect("list should succeed").0;
        assert!(listed.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn adding_a_nonexistent_instrument_is_404() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "watch-404").await;
        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        let result = add_watchlist_item(
            State(state),
            Extension(auth),
            Json(AddWatchlistItem { instrument_id: "999999999".to_string() }),
        )
        .await;

        assert!(matches!(result, Err(ApiError { status: StatusCode::NOT_FOUND, .. })));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn adding_the_same_instrument_twice_is_idempotent_not_an_error() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "watch-dup").await;
        let instrument_id = seed_instrument(&pool, "DUP").await.to_string();
        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        add_watchlist_item(State(state.clone()), Extension(auth.clone()), Json(AddWatchlistItem { instrument_id: instrument_id.clone() }))
            .await
            .expect("first add succeeds");
        add_watchlist_item(State(state), Extension(auth), Json(AddWatchlistItem { instrument_id }))
            .await
            .expect("second add is idempotent, not an error");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn one_principal_cannot_see_or_remove_anothers_watchlist_item() {
        let pool = test_pool().await;
        let (principal_a, code_a) = seed_principal(&pool, "watch-a").await;
        let (principal_b, code_b) = seed_principal(&pool, "watch-b").await;
        let instrument_id = seed_instrument(&pool, "XOWN").await.to_string();
        let state = test_app_state(pool);

        add_watchlist_item(
            State(state.clone()),
            Extension(AuthContext { principal_id: principal_a, principal_code: code_a.clone() }),
            Json(AddWatchlistItem { instrument_id: instrument_id.clone() }),
        )
        .await
        .expect("a adds");

        let b_list = list_watchlist(
            State(state.clone()),
            Extension(AuthContext { principal_id: principal_b, principal_code: code_b.clone() }),
        )
        .await
        .expect("list succeeds")
        .0;
        assert!(b_list.is_empty(), "b must not see a's watchlist item");

        // b's delete of an item that exists (for a, not b) must not error — it's
        // scoped to b's own rows, so it's a no-op, and a's row survives.
        remove_watchlist_item(
            State(state.clone()),
            Extension(AuthContext { principal_id: principal_b, principal_code: code_b }),
            Path(instrument_id.clone()),
        )
        .await
        .expect("no-op delete still succeeds");
        let a_list = list_watchlist(
            State(state),
            Extension(AuthContext { principal_id: principal_a, principal_code: code_a }),
        )
        .await
        .expect("list succeeds")
        .0;
        assert_eq!(a_list.len(), 1, "a's item must survive b's no-op delete");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn marks_returns_null_fields_for_unpriced_instruments_not_zero() {
        let pool = test_pool().await;
        let instrument_id = seed_instrument(&pool, "UNPRICED").await;
        let state = test_app_state(pool);
        // Deliberately: no MarkStore.set, no DailyStatsStore.set for this id.

        let result = get_marks(State(state), Query(MarksQuery { instrument_ids: instrument_id.to_string() }))
            .await
            .expect("should succeed even with nothing priced")
            .0;

        assert_eq!(result.len(), 1);
        assert!(result[0].bid.is_none());
        assert!(result[0].prev_close.is_none());
    }

    /// `daily_stats` and marks are populated independently (different feeds,
    /// different cadences) — one being present must never depend on, or be
    /// blocked by, the other. Here only `daily_stats` has data.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn marks_reports_prev_close_with_no_live_mark() {
        let pool = test_pool().await;
        let instrument_id = seed_instrument(&pool, "NOMARK").await;
        let state = test_app_state(pool);
        state.daily_stats().set(instrument_id, 100.0);
        // Deliberately: no MarkStore.set for this id.

        let result = get_marks(State(state), Query(MarksQuery { instrument_ids: instrument_id.to_string() }))
            .await
            .expect("should succeed with only daily_stats populated")
            .0;

        assert_eq!(result.len(), 1);
        assert!(result[0].bid.is_none());
        assert!(result[0].ask.is_none());
        assert!(result[0].mid.is_none());
        assert_eq!(result[0].prev_close, Some(100.0));
        assert!(result[0].pct_change.is_none());
    }

    // ── test plumbing ────────────────────────────────────────────────────────
    // Copied from `sessions.rs`'s test module, for the same reasons given there.

    /// `main` loads .env before resolving config; a test binary does not, so
    /// without this the test resolves a different database than the server runs
    /// against. The `oms` role carries `search_path = oms, public`
    /// (db/access/roles.sql); these are the admin credentials, so set it here.
    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// `principal.code` is `UNIQUE` and these tests leave their rows behind, so
    /// the code is suffixed with the row's own id — which is also what pushes it
    /// past 30 characters.
    async fn seed_principal(pool: &sqlx::PgPool, code: &str) -> (Uuid, String) {
        let id = Uuid::new_v4();
        let code = format!("{code}-{id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')",
        )
        .bind(id)
        .bind(&code)
        .execute(pool)
        .await
        .expect("seed principal");
        (id, code)
    }

    async fn seed_instrument(pool: &sqlx::PgPool, symbol_suffix: &str) -> i64 {
        let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("a seeded venue");
        let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("a seeded currency");
        sqlx::query_scalar(
            "INSERT INTO instrument \
                 (symbol, venue, name, asset_class, instrument_class, currency, status, \
                  price_precision, price_increment) \
             VALUES ($1, $2, 'Watchlist test instrument', 'EQUITY', 'SPOT', $3, 'ACTIVE', 2, 0.01) \
             RETURNING id",
        )
        .bind(format!("WATCH{symbol_suffix}{}", Uuid::new_v4().simple()))
        .bind(&venue)
        .bind(&currency)
        .fetch_one(pool)
        .await
        .expect("seed instrument")
    }

    /// Mirrors the existing actor-attribution test's `AppState::new(...)` call
    /// exactly (src/handlers.rs, the test just above this one) — empty registry,
    /// no Kafka, a throwaway quote channel. Nothing in these tests routes an
    /// order or reads the registry, so an empty one is correct, not a stub.
    fn test_app_state(pool: sqlx::PgPool) -> AppState {
        use crate::adapters::BrokerRegistry;
        use crate::stream_health::StreamHealthRegistry;
        use symbology::{Identifier, InMemoryCache, OpenFigiClient};

        let (quote_tx, _quote_rx) = tokio::sync::mpsc::channel(1);
        AppState::new(
            pool,
            "test-admin-token".to_string(),
            false,
            BrokerRegistry::new(),
            None,
            Identifier::new(OpenFigiClient::new(None), InMemoryCache::new()),
            StreamHealthRegistry::new(),
            None,
            quote_tx,
            crate::sessions::SessionConfig {
                cookie_policy: crate::sessions::cookie_policy("localhost:3001", None),
                ttl: crate::sessions::SessionTtl::default(),
                public_base_url: None,
            },
        )
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn venues_reports_eligible_and_ineligible_rows_with_reasons() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "venues-test").await;
        let instrument_id = seed_instrument(&pool, "VENUETEST").await;

        // Broker A: active connection, this portfolio HAS an account on it -> eligible.
        let broker_a = format!("VENUEA{}", Uuid::new_v4().simple());
        let conn_a = format!("venue-test-conn-a-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')",
        )
        .bind(&conn_a)
        .bind(&broker_a)
        .execute(&pool)
        .await
        .expect("seed broker_connection a");
        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'VENUETEST', true)",
        )
        .bind(instrument_id)
        .bind(&broker_a)
        .execute(&pool)
        .await
        .expect("seed broker_instrument a");

        let portfolio_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Venues test portfolio', 'ACTIVE')",
        )
        .bind(portfolio_id)
        .bind(format!("venues-test-portfolio-{portfolio_id}"))
        .execute(&pool)
        .await
        .expect("seed portfolio");
        sqlx::query(
            "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
             VALUES ($1, $2, $3, 'EXT', 'ACTIVE', $4)",
        )
        .bind(Uuid::new_v4())
        .bind(format!("venues-test-account-{portfolio_id}"))
        .bind(&conn_a)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed account");
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed grant");

        // Broker B: also cataloged, but no account for this portfolio -> ineligible.
        let broker_b = format!("VENUEB{}", Uuid::new_v4().simple());
        let conn_b = format!("venue-test-conn-b-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')",
        )
        .bind(&conn_b)
        .bind(&broker_b)
        .execute(&pool)
        .await
        .expect("seed broker_connection b");
        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'VENUETEST', true)",
        )
        .bind(instrument_id)
        .bind(&broker_b)
        .execute(&pool)
        .await
        .expect("seed broker_instrument b");

        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        let result = get_portfolio_venues(
            State(state),
            Extension(auth),
            Path(portfolio_id),
            Query(VenuesQuery { instrument_id: instrument_id.to_string() }),
        )
        .await
        .expect("should succeed")
        .0;

        assert_eq!(result.len(), 2);
        let a = result.iter().find(|v| v.broker_code == broker_a).expect("broker a present");
        assert!(a.eligible);
        assert_eq!(a.reason, None);
        let b = result.iter().find(|v| v.broker_code == broker_b).expect("broker b present");
        assert!(!b.eligible);
        assert_eq!(b.reason.as_deref(), Some("no account on this connection for this portfolio"));
    }

    /// `broker_connection` is UNIQUE(broker_code, environment), not UNIQUE(broker_code)
    /// — the same broker can have a PAPER and a LIVE connection side by side, and a
    /// single broker_instrument row (one per instrument+broker) must fan out to both.
    /// Guards against a future join "simplification" (DISTINCT ON broker_code, a
    /// scalar subquery, …) silently collapsing the two environments into one.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn venues_fans_out_across_environments_of_the_same_broker() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "venues-envs").await;
        let instrument_id = seed_instrument(&pool, "VENUEENV").await;

        let broker = format!("VENUEENV{}", Uuid::new_v4().simple());
        let conn_paper = format!("venue-env-conn-paper-{}", Uuid::new_v4());
        let conn_live = format!("venue-env-conn-live-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')",
        )
        .bind(&conn_paper)
        .bind(&broker)
        .execute(&pool)
        .await
        .expect("seed broker_connection paper");
        sqlx::query(
            "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'LIVE', 'ACTIVE')",
        )
        .bind(&conn_live)
        .bind(&broker)
        .execute(&pool)
        .await
        .expect("seed broker_connection live");
        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'VENUEENV', true)",
        )
        .bind(instrument_id)
        .bind(&broker)
        .execute(&pool)
        .await
        .expect("seed broker_instrument");

        let portfolio_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Venues envs test portfolio', 'ACTIVE')",
        )
        .bind(portfolio_id)
        .bind(format!("venues-envs-portfolio-{portfolio_id}"))
        .execute(&pool)
        .await
        .expect("seed portfolio");
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed grant");

        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };

        let result = get_portfolio_venues(
            State(state),
            Extension(auth),
            Path(portfolio_id),
            Query(VenuesQuery { instrument_id: instrument_id.to_string() }),
        )
        .await
        .expect("should succeed")
        .0;

        assert_eq!(result.len(), 2, "one row per environment, not one per broker_code");
        assert!(result.iter().all(|v| v.broker_code == broker));
        let paper = result.iter().find(|v| v.environment.as_deref() == Some("PAPER")).expect("paper row present");
        assert_eq!(paper.broker_connection_code.as_deref(), Some(conn_paper.as_str()));
        let live = result.iter().find(|v| v.environment.as_deref() == Some("LIVE")).expect("live row present");
        assert_eq!(live.broker_connection_code.as_deref(), Some(conn_live.as_str()));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn venues_for_an_uncataloged_instrument_is_an_empty_list_not_an_error() {
        let pool = test_pool().await;
        let (principal_id, principal_code) = seed_principal(&pool, "venues-empty").await;
        let portfolio_id = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Empty test portfolio', 'ACTIVE')")
            .bind(portfolio_id)
            .bind(format!("venues-empty-portfolio-{portfolio_id}"))
            .execute(&pool)
            .await
            .expect("seed portfolio");
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view) VALUES ($1, $2, $3, true)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&pool)
        .await
        .expect("seed grant");

        let state = test_app_state(pool);
        let auth = AuthContext { principal_id, principal_code };
        let result = get_portfolio_venues(
            State(state),
            Extension(auth),
            Path(portfolio_id),
            Query(VenuesQuery { instrument_id: "999999999".to_string() }),
        )
        .await
        .expect("should succeed")
        .0;

        assert!(result.is_empty());
    }
}




 
