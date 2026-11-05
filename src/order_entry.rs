//! The one order path. REST (`handlers::orders_submit` / `orders_cancel`), the
//! FIX acceptor for strategies and Flatten all submit and cancel through here,
//! so every order gets the same checks and is booked the same way.

use axum::http::StatusCode;
use chrono::Utc;
use sqlx::{query_scalar, Row};
use tracing::{error, info, warn};
use uuid::Uuid;

use gateway::adapter::{BrokerError, BrokerOrderRequest};

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{CancelOrder, OrderCommand, RouteOrder};
use crate::domain::orders::state::{OrderAggregateState, TimeInForce};
use crate::event_bus::publish_appended;
use crate::event_store::OrderEventStore;
use crate::handlers::{
    actor_for, domain_event_to_new_event, map_rejection_to_api_error, parse_order_side, parse_order_status,
    parse_order_type, parse_time_in_force, require_order_grant, resolve_instrument_ref, ApiError, OrderPermission,
    SubmitOrderRequest,
};
use crate::domain::orders::risk::RiskRejection;
use crate::risk_engine::{PgRiskDataProvider, RiskCheckError, RiskEngine};
use crate::strategies::{self, guard, Strategy};

/// Where an order came from. Recorded on the order (`order_state.origin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Api,
    Fix { cl_ord_id: String },
    Flatten,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Api => "API",
            Origin::Fix { .. } => "FIX",
            Origin::Flatten => "FLATTEN",
        }
    }
}

/// A pre-trade rejection as the API reports it; the reason names the check.
fn risk_api_error(order_id: Uuid, rejection: RiskRejection) -> ApiError {
    warn!(order_id = %order_id, code = %rejection.code, message = %rejection.message, "order rejected by pre-trade risk");
    // TODO: also persist an OrderDenied audit event for rejected orders.
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: format!("risk check failed [{}]: {}", rejection.code, rejection.message),
    }
}

/// For a strategy order naming its instrument by broker symbol plus broker
/// code: (the strategy's account for that broker, the instrument id).
/// `None` when the order names its instrument another way.
async fn strategy_route(pool: &sqlx::PgPool, strategy: &Strategy, req: &SubmitOrderRequest) -> Result<Option<(Uuid, i64)>, ApiError> {
    let (None, Some(symbol), Some(venue)) = (&req.instrument_id, &req.symbol, &req.venue) else {
        return Ok(None);
    };
    let unprocessable = |message: String| ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, message };
    let account = *strategy
        .connections
        .get(venue)
        .ok_or_else(|| unprocessable(format!("strategy has no connection for {venue}")))?;
    let instrument_id: i64 = query_scalar(
        "SELECT instrument_id FROM broker_instrument WHERE broker_symbol = $1 AND broker_code = $2 AND is_tradeable = true",
    )
    .bind(symbol)
    .bind(venue)
    .fetch_optional(pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to resolve broker symbol: {:?}", err),
    })?
    .ok_or_else(|| unprocessable(format!("{symbol} is not tradeable on {venue}")))?;
    Ok(Some((account, instrument_id)))
}

/// What a cancel did: asked the broker (it confirms on the execution stream),
/// or cancelled an order that never reached one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Requested,
    Canceled,
}

/// Validate, risk-check, store and route one order; returns its OMS order id.
pub async fn submit(state: &AppState, auth: &AuthContext, req: SubmitOrderRequest, origin: Origin) -> Result<Uuid, ApiError> {
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
    // A strategy's portfolio is traded only by the strategy. Its orders may name
    // the instrument by broker symbol plus broker code (`venue`), which also
    // picks the strategy's account for that broker.
    let strategy = state.strategies().by_portfolio_id(portfolio_id);
    if let Some(s) = &strategy {
        guard::may_trade(s, auth.principal_id).map_err(|r| risk_api_error(order_id, r))?;
    }
    let strategy_route = match &strategy {
        Some(s) => strategy_route(&pool, s, &req).await?,
        None => None,
    };

    // Resolve the account: explicit override if given, else the portfolio's default route.
    // Captured before `req` is moved into `cmd` below; drives the ownership
    // check further down (an explicit account_id must belong to portfolio_id).
    let account_id_was_explicit = req.account_id.is_some() || strategy_route.is_some();
    let account_id: Uuid = match (strategy_route, req.account_id.as_deref()) {
        (Some((account, _)), _) => account,
        (None, Some(a)) => Uuid::parse_str(a).map_err(|_| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "account_id must be a UUID".to_string(),
        })?,
        (None, None) => query_scalar::<_, Option<Uuid>>(
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
    let instrument_id_bigint = match strategy_route {
        Some((_, instrument_id)) => instrument_id,
        None => resolve_instrument_ref(&pool, &req).await?,
    };
    let fix_cl_ord_id = match &origin {
        Origin::Fix { cl_ord_id } => Some(cl_ord_id.clone()),
        _ => None,
    };

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
        "SELECT instrument_class, status, symbol, asset_class, venue, currency, \
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

    // A strategy order's reference price, looked up before the transaction so
    // waiting for a first quote never holds the strategy's row lock.
    let quote_currency: String = instrument_row.get("currency");
    let mark = match &strategy {
        Some(s) => {
            let inst = dataprovider::InstrumentRef {
                id: instrument_id_bigint,
                symbol: instrument_row.get("symbol"),
                instrument_class: Some(instrument_class.clone()),
                asset_class: instrument_row.get("asset_class"),
                venue: instrument_row.get("venue"),
            };
            guard::reference(state, s, &inst).await.map_err(|r| risk_api_error(order_id, r))?
        }
        None => None,
    };

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
    let risk = if strategy.is_some() {
        // The strategy row as it is now, locked: its state and limits may have
        // changed since the snapshot (Kill), and the lock serializes its submits.
        match strategies::by_portfolio(&mut tx, portfolio_id).await {
            Ok(Some(locked)) => guard::check(&mut tx, &locked, &cmd, &quote_currency, mark, contract_size).await,
            Ok(None) => Ok(()),
            Err(e) => Err(RiskCheckError::Data(e)),
        }
    } else {
        RiskEngine::new(PgRiskDataProvider::new(&mut *tx)).check_submit(portfolio_id, &cmd, contract_size).await
    };
    risk.map_err(|err| match err {
        RiskCheckError::Rejected(rejection) => risk_api_error(order_id, rejection),
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
            broker_connection_code,
            origin,
            fix_cl_ord_id
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20
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
    .bind(origin.as_str())
    .bind(&fix_cl_ord_id)
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
    let appended = event_store
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

    publish_appended(state.event_bus(), order_id, &new_events, &appended);

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

        if current_state.status.is_terminal() {
            // The broker already reported a fill (or the order was canceled
            // concurrently) before we got here — same race MAX_ROUTE_ATTEMPTS
            // guards against, just resolved in execution.rs's favor entirely.
            // RouteOrder is meaningless on a closed order; the broker call
            // already succeeded and the terminal event is already persisted,
            // so this is success, not a failure to report to the client.
            info!(
                order_id = %order_id,
                status = ?current_state.status,
                "order reached a terminal state before routing could persist (raced a concurrent fill/cancel) — treating as routed"
            );
            break;
        }

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

        let appended = match event_store
            .append_events_in_tx(&mut tx2, order_id, expected_version, &route_new_events)
            .await
        {
            Ok(r) => r,
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
        };

        tx2.commit().await.map_err(|err| {
            error!(order_id = %order_id, error = ?err, "TX2 commit failed — broker holds order but OMS status is Submitted");
            ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to commit routing transaction: {:?}", err),
            }
        })?;
        publish_appended(state.event_bus(), order_id, &route_new_events, &appended);
        break;
    }


    Ok(order_id)
}

/// Cancel one order, on the caller's `can_trade` grant.
pub async fn cancel(app_state: &AppState, auth: &AuthContext, order_id: Uuid, reason: Option<String>) -> Result<CancelOutcome, ApiError> {
    info!(%order_id, principal_id = %auth.principal_id, "cancel order received");

    let pool = app_state.pool().clone();
    let event_store = OrderEventStore::new(pool.clone());


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
        return Ok(CancelOutcome::Requested);
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
        .decide(OrderCommand::CancelOrder(CancelOrder { order_id: order_id.to_string(), reason }), metadata)
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

    let appended = event_store
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

    publish_appended(app_state.event_bus(), order_id, &new_events, &appended);

    // return response
    Ok(CancelOutcome::Canceled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::test_support;

    fn request(portfolio_id: Uuid, symbol: &str, venue: &str) -> SubmitOrderRequest {
        serde_json::from_value(serde_json::json!({
            "order_id": Uuid::new_v4().to_string(),
            "client_order_id": "leg",
            "portfolio_id": portfolio_id.to_string(),
            "symbol": symbol,
            "venue": venue,
            "side": "buy",
            "order_type": "limit",
            "time_in_force": "gtc",
            "limit_price": 1.0,
            "quantity": 1.0,
        }))
        .unwrap()
    }

    /// Against the shared dev DB (read-only): the two legs of an arbitrage name
    /// different brokers and resolve to each broker's own account, and a broker
    /// the strategy has no connection for is refused with its reason.
    #[tokio::test]
    #[ignore]
    async fn two_legs_resolve_to_their_own_accounts() {
        let pool = test_support::dev_pool().await;
        let mut s = crate::strategies::tests::sample("s", "S", "OMS");
        let (binance, bybit) = (Uuid::new_v4(), Uuid::new_v4());
        s.connections.insert("BINANCE".into(), binance);
        s.connections.insert("BYBIT".into(), bybit);
        let leg1 = strategy_route(&pool, &s, &request(s.portfolio_id, "BTCUSDT", "BINANCE")).await.unwrap().unwrap();
        let leg2 = strategy_route(&pool, &s, &request(s.portfolio_id, "BTCUSDT", "BYBIT")).await.unwrap().unwrap();
        assert_eq!((leg1.0, leg2.0), (binance, bybit));
        let e = strategy_route(&pool, &s, &request(s.portfolio_id, "BTCUSDT", "OKX")).await.unwrap_err();
        assert_eq!(e.message, "strategy has no connection for OKX");
    }

    /// Against the shared dev DB: a REST order on a strategy's portfolio from
    /// anyone but the strategy is refused before anything is stored.
    #[tokio::test]
    #[ignore]
    async fn a_rest_order_on_a_strategy_portfolio_from_another_principal_is_refused() {
        let pool = test_support::dev_pool().await;
        let code = test_support::seed_strategy(&pool).await;
        let state = test_support::state(pool.clone());
        state.strategies().reload(&pool).await.unwrap();
        let s = state.strategies().by_code(&code).unwrap();
        let other = AuthContext { principal_id: Uuid::new_v4(), principal_code: "someone".into() };
        let e = submit(&state, &other, request(s.portfolio_id, "BTCUSDT", "BINANCE"), Origin::Api).await.unwrap_err();
        assert!(e.message.contains("strategy_portfolio"), "{}", e.message);
        test_support::drop_strategy(&pool, &code).await;
    }
}
