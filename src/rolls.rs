//! The one-click futures roll (`POST /orders/roll`).
//!
//! A roll is one **parent** order on a calendar-spread instrument (`ESZ6-ESH7`),
//! which is what goes to the broker (as one combo order), plus two ordinary
//! **child** orders, one per leg, which are never sent on their own and book the
//! leg fills. Positions, replay and the fill shape therefore stay per instrument.
//! The children are risk-checked like any order (a refusal of either refuses all
//! three); the parent is not. A child is cancelled only through its parent.

use axum::{extract::State, http::StatusCode, Extension, Json};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::{info, warn};
use uuid::Uuid;

use gateway::adapter::{BrokerLeg, BrokerOrderRequest};

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::domain::orders::commands::SubmitOrder;
use crate::domain::orders::risk::RiskRejection;
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};
use crate::event_bus::publish_appended;
use crate::event_store::OrderEventStore;
use crate::handlers::{internal, map_rejection_to_api_error, ApiError};
use crate::order_entry::{self, Origin, RouteResult, Routing};
use crate::risk_engine::{PgRiskDataProvider, RiskCheckError, RiskEngine};

/// Wire payload for `POST /orders/roll`.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct RollRequest {
    pub portfolio_id: Uuid,
    /// The held contract (the near leg).
    pub from_instrument_id: i64,
    /// A later contract of the same root (the far leg).
    pub to_instrument_id: i64,
    /// Contracts to roll: more than 0, at most the position.
    #[schema(example = 4.0)]
    pub qty: f64,
    pub order_type: OrderType,
    /// For a `limit` roll: the net debit per spread, i.e. the buy leg's price
    /// minus the sell leg's (the price of a combo bought with the legs' own
    /// directions). May be zero or negative (e.g. a short roll in contango is
    /// usually a credit). Omit for `market`.
    #[schema(example = json!(null))]
    pub limit_price: Option<f64>,
    /// `day` only.
    pub time_in_force: TimeInForce,
}

/// The orders a roll created: the parent and its children (near leg, far leg).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct RollIds {
    pub parent_order_id: Uuid,
    #[schema(value_type = Vec<Uuid>)]
    pub child_order_ids: [Uuid; 2],
}

fn unprocessable(message: String) -> ApiError {
    ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, message }
}

/// The spread instrument for `near_id`/`far_id`, created on first use:
/// symbol `<NEAR>-<FAR>`, venue `XCME`, class `FUTURE`, the near leg's tick,
/// precision, currency and contract size; no derivative row and no
/// broker_instrument row (routing builds the combo from the legs).
pub async fn ensure_spread(pool: &PgPool, near_id: i64, far_id: i64) -> Result<i64, sqlx::Error> {
    if let Some(id) = sqlx::query_scalar("SELECT instrument_id FROM future_spread WHERE near_id = $1 AND far_id = $2")
        .bind(near_id)
        .bind(far_id)
        .fetch_optional(pool)
        .await?
    {
        return Ok(id);
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, \
                                 price_precision, size_precision, price_increment, size_increment, contract_size) \
         SELECT n.symbol || '-' || f.symbol, 'XCME', n.symbol || '-' || f.symbol || ' calendar spread', \
                n.asset_class, 'FUTURE', n.currency, 'ACTIVE', \
                n.price_precision, n.size_precision, n.price_increment, n.size_increment, n.contract_size \
         FROM instrument n, instrument f WHERE n.id = $1 AND f.id = $2 \
         ON CONFLICT (symbol, venue) DO NOTHING",
    )
    .bind(near_id)
    .bind(far_id)
    .execute(&mut *tx)
    .await?;
    let id: i64 = sqlx::query_scalar(
        "SELECT i.id FROM instrument i, instrument n, instrument f \
         WHERE n.id = $1 AND f.id = $2 AND i.symbol = n.symbol || '-' || f.symbol AND i.venue = 'XCME'",
    )
    .bind(near_id)
    .bind(far_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO future_spread (instrument_id, near_id, far_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(near_id)
        .bind(far_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    // A concurrent first roll may have won either insert: the table is the answer.
    sqlx::query_scalar("SELECT instrument_id FROM future_spread WHERE near_id = $1 AND far_id = $2")
        .bind(near_id)
        .bind(far_id)
        .fetch_one(pool)
        .await
}

/// One leg contract as a roll needs it.
struct Leg {
    id: i64,
    symbol: String,
    status: String,
    class: String,
    root: Option<String>,
    expiry: Option<NaiveDate>,
    contract_size: f64,
}

async fn leg(pool: &PgPool, id: i64) -> Result<Leg, ApiError> {
    let row = sqlx::query(
        "SELECT i.id, i.symbol, i.status, i.instrument_class, d.underlying_symbol, d.expiry_date, \
                i.contract_size::float8 AS contract_size \
         FROM instrument i LEFT JOIN instrument_derivative d ON d.instrument_id = i.id WHERE i.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| internal("failed to load roll instrument", e))?
    .ok_or_else(|| unprocessable(format!("instrument {id} not found")))?;
    Ok(Leg {
        id: row.get("id"),
        symbol: row.get("symbol"),
        status: row.get("status"),
        class: row.get("instrument_class"),
        root: row.get("underlying_symbol"),
        expiry: row.get("expiry_date"),
        contract_size: row.get("contract_size"),
    })
}

/// A leg's price for its risk check: the live mid, when there is a fresh one.
/// Without one the notional limits are skipped, as for any market order.
fn leg_price(state: &AppState, instrument_id: i64) -> Option<f64> {
    state
        .marks()
        .get(instrument_id)
        .filter(|m| chrono::Utc::now().signed_duration_since(m.ts).to_std().map_or(true, |age| age <= crate::strategies::guard::MAX_MARK_AGE))
        .map(|m| m.mid())
}

/// Validate, risk-check, store and route one roll. See the module doc.
pub async fn submit_roll(state: &AppState, auth: &AuthContext, req: RollRequest) -> Result<RollIds, ApiError> {
    info!(?req, principal_id = %auth.principal_id, "roll received");
    crate::authz::require_mfa(auth)?;
    let pool = state.pool().clone();
    let pf = req.portfolio_id;

    crate::authz::check(&pool, auth.principal_id, crate::authz::Permission::OrdersCreate, pf)
        .await
        .map_err(|e| internal("failed to check grant", e))?
        .map_err(|denied| denied.into_api("unauthorized"))?;
    if !(req.qty.is_finite() && req.qty > 0.0) {
        return Err(unprocessable("roll quantity must be more than 0".into()));
    }
    if req.time_in_force != TimeInForce::Day {
        return Err(unprocessable("a roll is a day order (time_in_force=day)".into()));
    }
    crate::domain::orders::invariants::validate_priced_submit(req.qty, req.order_type, req.limit_price, true)
        .map_err(map_rejection_to_api_error)?;
    // A strategy's portfolio is traded by its strategy only (and a reported
    // one not through the OMS at all).
    let strategy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM strategy WHERE portfolio_id = $1)")
        .bind(pf)
        .fetch_one(&pool)
        .await
        .map_err(|e| internal("database error", e))?;
    if strategy {
        return Err(ApiError { status: StatusCode::CONFLICT, message: "a strategy portfolio cannot be rolled here".into() });
    }

    // The legs: two ACTIVE dated futures of one root, `to` expiring later.
    let (from, to) = (leg(&pool, req.from_instrument_id).await?, leg(&pool, req.to_instrument_id).await?);
    let (Some(root), Some(from_expiry), "FUTURE") = (from.root.clone(), from.expiry, from.class.as_str()) else {
        return Err(unprocessable(format!("{} is not a dated future", from.symbol)));
    };
    if to.class != "FUTURE" || to.root.as_deref() != Some(root.as_str()) || to.expiry.map_or(true, |e| e <= from_expiry) {
        return Err(unprocessable(format!("{} is not a later {root} contract than {}", to.symbol, from.symbol)));
    }
    for l in [&from, &to] {
        if l.status != "ACTIVE" {
            return Err(unprocessable(format!("{} is not active", l.symbol)));
        }
    }

    // The position decides the sides: a long sells the near leg and buys the far.
    let net: f64 = sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
        .bind(pf)
        .bind(from.id.to_string())
        .fetch_optional(&pool)
        .await
        .map_err(|e| internal("failed to load the position", e))?
        .unwrap_or(0.0);
    if net == 0.0 {
        return Err(unprocessable(format!("no position in {} to roll", from.symbol)));
    }
    if req.qty > net.abs() {
        return Err(unprocessable(format!("roll quantity {} exceeds the position of {}", req.qty, net.abs())));
    }
    let (from_side, to_side) = if net > 0.0 { (OrderSide::Sell, OrderSide::Buy) } else { (OrderSide::Buy, OrderSide::Sell) };

    // The venue: the account the portfolio would trade the held contract through.
    let grants = {
        let mut conn = pool.acquire().await.map_err(|e| internal("database error", e))?;
        crate::grants::store::active_for_portfolio(&mut conn, pf)
            .await
            .map_err(|e| internal("failed to load trading grants", e))?
    };
    let account_id = order_entry::granted_account(&pool, pf, from.id, None, &grants).await?;
    let route = order_entry::account_route(&pool, account_id).await?;
    let facts = crate::grants::OrderFacts {
        connection: route.broker_connection_code.clone(),
        broker_code: route.broker_code.clone(),
        instrument_class: Some("FUTURE".into()),
        at: chrono::Utc::now(),
    };
    crate::grants::check(&facts, &grants).map_err(|crate::grants::Denied(message)| unprocessable(message))?;
    order_entry::connector_trades(&route.broker_code, "FUTURE")?;
    // Each leg's broker-native id (an IBKR conId): the combo is built from them.
    let mut native_ids = Vec::with_capacity(2);
    for l in [&from, &to] {
        // (min_quantity, native_id) when listed and tradeable.
        let listed: Option<(Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT min_quantity::float8, native_id FROM broker_instrument \
             WHERE instrument_id = $1 AND broker_code = $2 AND is_tradeable = true",
        )
        .bind(l.id)
        .bind(&route.broker_code)
        .fetch_optional(&pool)
        .await
        .map_err(|e| internal("failed to validate broker instrument mapping", e))?;
        match listed {
            None => return Err(unprocessable(format!("no tradeable mapping for {} on broker {}", l.symbol, route.broker_code))),
            Some((Some(min), _)) if req.qty < min => {
                return Err(unprocessable(format!("quantity {} below broker minimum {min} for {}", req.qty, l.symbol)));
            }
            // No native id: the adapter refuses the leg with its own message.
            Some((_, native_id)) => native_ids.push(native_id.unwrap_or_default()),
        }
    }

    let spread = ensure_spread(&pool, from.id, to.id).await.map_err(|e| internal("failed to create the spread instrument", e))?;
    let spread_symbol = format!("{}-{}", from.symbol, to.symbol);

    // The three orders. The parent carries the spread price; the children are
    // unpriced (they fill at the leg prices of the one combo order).
    let parent_id = Uuid::new_v4();
    let child_ids = [Uuid::new_v4(), Uuid::new_v4()];
    let order = |order_id: Uuid, client: String, instrument: i64, side, order_type, limit_price, parent: Option<Uuid>| SubmitOrder {
        order_id: order_id.to_string(),
        client_order_id: client,
        portfolio_id: pf.to_string(),
        account_id: account_id.to_string(),
        instrument_id: instrument.to_string(),
        side,
        order_type,
        time_in_force: req.time_in_force,
        limit_price,
        quantity: req.qty,
        parent_order_id: parent.map(|p| p.to_string()),
        // only the parent is priced, as a net spread price
        spread_price: parent.is_none(),
    };
    let parent_cmd = order(parent_id, parent_id.to_string(), spread, OrderSide::Buy, req.order_type, req.limit_price, None);
    let legs = [(&from, from_side), (&to, to_side)];
    let child_cmds: Vec<SubmitOrder> = legs
        .iter()
        .zip(child_ids)
        .enumerate()
        .map(|(i, ((l, side), id))| order(id, format!("{parent_id}-{}", i + 1), l.id, *side, OrderType::Market, None, Some(parent_id)))
        .collect();

    // TX1: risk on each child, then all three orders, or none.
    let mut tx = pool.begin().await.map_err(|e| internal("failed to start transaction", e))?;
    // One roll of a (portfolio, contract) at a time, and only what is not already
    // being closed: a second click or a retry must not roll the position twice
    // (and so flip it). Working (or suspended) closing-side orders on `from`, legs of rolls in
    // flight included, use up the position.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("roll:{pf}:{}", from.id))
        .execute(&mut *tx)
        .await
        .map_err(|e| internal("failed to lock the position for the roll", e))?;
    let (net_now, closing): (f64, f64) = sqlx::query_as(
        "SELECT COALESCE((SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2), 0)::float8, \
                COALESCE((SELECT SUM(leaves_qty)::float8 FROM order_state WHERE portfolio_id = $1 AND instrument_id = $2 \
                          AND side = $3 AND status IN ('submitted', 'routed', 'partially_filled', 'suspended')), 0)::float8",
    )
    .bind(pf)
    .bind(from.id.to_string())
    .bind(from_side.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| internal("failed to load the position", e))?;
    let free = if net_now * net > 0.0 { (net_now.abs() - closing).max(0.0) } else { 0.0 };
    if req.qty > free {
        return Err(unprocessable(format!(
            "roll quantity {} exceeds the {free} {} not already being closed",
            req.qty, from.symbol
        )));
    }
    let mut refused: Option<(usize, RiskRejection)> = None;
    for (i, cmd) in child_cmds.iter().enumerate() {
        let l = legs[i].0;
        match RiskEngine::new(PgRiskDataProvider::new(&mut tx)).check_submit_at(pf, cmd, l.contract_size, leg_price(state, l.id)).await {
            Ok(()) => {}
            Err(RiskCheckError::Rejected(r)) => {
                refused = Some((i, r));
                break;
            }
            Err(RiskCheckError::Data(e)) => return Err(internal("failed to evaluate pre-trade risk", e)),
        }
    }
    let store = OrderEventStore::new(pool.clone());
    let conn_code = route.broker_connection_code.as_str();
    // (order id, parent, command, reject reason when refused)
    let mut orders = vec![(parent_id, None, parent_cmd, None)];
    for (j, cmd) in child_cmds.into_iter().enumerate() {
        orders.push((child_ids[j], Some(parent_id), cmd, None));
    }
    if let Some((i, rejection)) = &refused {
        // Release the risk locks, then keep all three refusals on record.
        drop(tx);
        tx = pool.begin().await.map_err(|e| internal("failed to start transaction", e))?;
        let why = format!("{}: {}", rejection.code, rejection.message);
        let leg_why = format!("roll refused: leg {} {why}", legs[*i].0.symbol);
        for (k, o) in orders.iter_mut().enumerate() {
            o.3 = Some(if k == i + 1 { why.clone() } else { leg_why.clone() });
        }
    }
    let mut stored = Vec::with_capacity(3);
    for (id, parent, cmd, rejected) in orders {
        let (s, events) = match rejected {
            Some(reason) => order_entry::rejected_order(auth, id, cmd, reason)?,
            None => order_entry::submitted_order(auth, cmd)?,
        };
        order_entry::insert_order_state(&mut tx, id, &s, auth.principal_id, conn_code, &Origin::Api, &None, parent).await?;
        stored.push((id, events));
    }
    let mut appended = Vec::with_capacity(3);
    for (id, events) in &stored {
        let a = store.append_events_in_tx(&mut tx, *id, 0, events).await.map_err(|e| internal("failed to append order audit event", e))?;
        appended.push(a);
    }
    tx.commit().await.map_err(|e| internal("failed to commit transaction", e))?;
    for ((id, events), a) in stored.iter().zip(&appended) {
        publish_appended(state.event_bus(), *id, events, a);
    }
    if let Some((_, rejection)) = refused {
        return Err(order_entry::risk_api_error(parent_id, rejection));
    }

    // TX2: only the parent goes to the broker, as one combo order carrying the
    // legs with their own directions (ratio 1 each). The parent is a BUY at the
    // net price as entered; see `RollRequest::limit_price` and the IBKR
    // adapter's `submit_bag` for the convention.
    let broker_legs = legs
        .iter()
        .zip(native_ids)
        .map(|((_, side), native_id)| BrokerLeg { native_id, side: side.as_str().to_string(), ratio: 1 })
        .collect();
    let broker_req = BrokerOrderRequest {
        order_id: parent_id.to_string(),
        symbol: spread_symbol,
        native_id: None,
        quantity: req.qty,
        side: OrderSide::Buy.as_str().to_string(),
        order_type: req.order_type.as_str().to_string(),
        time_in_force: req.time_in_force.as_str().to_string(),
        limit_price: req.limit_price,
        external_account_ref: route.external_account_ref.clone(),
        legs: broker_legs,
    };
    match order_entry::route_stored(state, auth, parent_id, &route.broker_code, &route.environment, &broker_req, "").await? {
        Routing::Taken(RouteResult::Terminal(status), _) => {
            info!(parent_order_id = %parent_id, ?status, "roll closed while routing; its legs follow the parent");
        }
        Routing::Taken(_, external_order_id) => {
            route_children(&pool, state.event_bus(), parent_id, &route.broker_code, &external_order_id, &crate::handlers::actor_for(auth)).await?;
        }
        Routing::Rejected(err) => {
            for id in child_ids {
                order_entry::book_reject(state, id, format!("roll refused: {}", err.message)).await;
            }
            return Err(err);
        }
        Routing::Unknown(err) => return Err(err),
    }
    Ok(RollIds { parent_order_id: parent_id, child_order_ids: child_ids })
}

/// Mark the legs of `parent_id` routed: the venue took the parent as
/// `external_order_id`. Legs no longer `submitted` are left as they are.
pub async fn route_children(
    pool: &PgPool,
    bus: &crate::event_bus::EventBus,
    parent_id: Uuid,
    venue: &str,
    external_order_id: &str,
    actor: &str,
) -> Result<(), ApiError> {
    let legs: Vec<Uuid> = sqlx::query_scalar("SELECT order_id FROM order_state WHERE parent_order_id = $1 ORDER BY client_order_id")
        .bind(parent_id)
        .fetch_all(pool)
        .await
        .map_err(|e| internal("failed to load leg orders", e))?;
    for id in legs {
        let result = order_entry::persist_leg_routed(pool, bus, id, venue, external_order_id, actor).await?;
        if result != RouteResult::Routed {
            warn!(order_id = %id, parent_order_id = %parent_id, ?result, "roll leg not marked routed");
        }
    }
    Ok(())
}

/// Roll a futures position into a later contract
#[utoipa::path(
    post, path = "/orders/roll", tag = "orders",
    request_body = RollRequest,
    responses(
        (status = 200, description = "Roll accepted: the parent (spread) order was routed; its two leg orders book the fills", body = RollIds),
        (status = 400, description = "Validation error"),
        (status = 403, description = "No trade grant for principal/portfolio"),
        (status = 409, description = "Strategy portfolio"),
        (status = 422, description = "No such position, quantity above it, not a later contract of the same root, no tradeable mapping, or rejected by pre-trade risk (then the roll and both legs are rejected)"),
        (status = 502, description = "Broker rejected the roll, or did not answer"),
        (status = 503, description = "No broker adapter configured"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn orders_roll(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<RollRequest>,
) -> Result<Json<RollIds>, ApiError> {
    Ok(Json(submit_roll(&state, &auth, req).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_entry::tests::{g_connection, g_defaults, g_trader, grant_ibkr};
    use crate::order_entry::{cancel, CancelOutcome};
    use crate::strategies::test_support;
    use gateway::adapter::{BrokerAdapter, BrokerError, BrokerLeg, BrokerOrderRequest, BrokerOrderResponse, BrokerRegistry};
    use std::sync::{Arc, Mutex};

    /// A broker that records what it was sent and answers as told.
    struct Bag {
        sent: Mutex<Vec<(String, String, f64)>>,
        /// The legs of each request sent.
        legs: Mutex<Vec<Vec<BrokerLeg>>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl BrokerAdapter for Bag {
        async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
            self.sent.lock().unwrap().push((req.order_id.clone(), req.side.clone(), req.quantity));
            self.legs.lock().unwrap().push(req.legs.clone());
            if self.fail {
                return Err(BrokerError::Network("timeout".into()));
            }
            Ok(BrokerOrderResponse { external_order_id: format!("B{}", self.sent.lock().unwrap().len()) })
        }
        async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> {
            Ok(())
        }
    }

    struct World {
        pool: PgPool,
        state: AppState,
        pf: Uuid,
        me: AuthContext,
        near: i64,
        far: i64,
        bag: Arc<Bag>,
    }

    async fn future(pool: &PgPool, symbol: &str, root: &str, expiry: &str, con_id: &str) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, contract_size, price_precision, price_increment) \
             VALUES ($1, 'XCME', $1, 'INDEX', 'FUTURE', 'USD', 50, 2, 0.25) RETURNING id",
        )
        .bind(symbol)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO instrument_derivative (instrument_id, underlying_symbol, expiry_date) VALUES ($1, $2, $3::date)")
            .bind(id)
            .bind(root)
            .bind(expiry)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, native_id, is_tradeable) VALUES ($1, 'IBKR_TWS', $2, $3, true)")
            .bind(id)
            .bind(symbol)
            .bind(con_id)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn world(net_qty: f64, fail: bool) -> World {
        let pool = crate::authz_schema_tests::migrated_db().await;
        let state = test_support::state(pool.clone());
        let pf = crate::groups_api::tests::portfolio(&pool).await;
        let me = g_trader(&pool, pf).await;
        let near = future(&pool, "ESZ6", "ES", "2026-12-18", "1001").await;
        let far = future(&pool, "ESH7", "ES", "2027-03-19", "1002").await;
        let tws = g_connection(&pool, "IBKR_TWS").await;
        grant_ibkr(&pool, pf, &[&tws]).await;
        g_defaults(&pool, &[crate::access_grants::GrantKey::Orders]).await;
        if net_qty != 0.0 {
            sqlx::query("INSERT INTO position (portfolio_id, instrument_id, net_qty) VALUES ($1, $2, $3)")
                .bind(pf)
                .bind(near.to_string())
                .bind(net_qty)
                .execute(&pool)
                .await
                .unwrap();
        }
        let bag = Arc::new(Bag { sent: Mutex::default(), legs: Mutex::default(), fail });
        let mut registry = BrokerRegistry::new();
        registry.register("IBKR_TWS", "PAPER", bag.clone());
        state.swap_registries(registry, Default::default());
        World { pool, state, pf, me, near, far, bag }
    }

    fn roll(w: &World, qty: f64) -> RollRequest {
        RollRequest {
            portfolio_id: w.pf,
            from_instrument_id: w.near,
            to_instrument_id: w.far,
            qty,
            order_type: OrderType::Limit,
            limit_price: Some(12.25),
            time_in_force: TimeInForce::Day,
        }
    }

    /// (instrument, side, qty, status, parent) of each order of the portfolio's
    /// rolls, children ordered near leg first.
    async fn children(pool: &PgPool, parent: Uuid) -> Vec<(i64, String, f64, String, Option<Uuid>)> {
        sqlx::query_as(
            "SELECT instrument_id::bigint, side, original_qty::float8, status, parent_order_id FROM order_state \
             WHERE parent_order_id = $1 ORDER BY client_order_id",
        )
        .bind(parent)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn status(pool: &PgPool, order_id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM order_state WHERE order_id = $1").bind(order_id).fetch_one(pool).await.unwrap()
    }

    async fn order_count(pool: &PgPool, pf: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM order_state WHERE portfolio_id = $1").bind(pf).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn long_roll_creates_sell_near_buy_far_children() {
        let w = world(4.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 4.0)).await.unwrap();
        let kids = children(&w.pool, ids.parent_order_id).await;
        let p = Some(ids.parent_order_id);
        assert_eq!(
            kids,
            vec![
                (w.near, "sell".to_string(), 4.0, "routed".to_string(), p),
                (w.far, "buy".to_string(), 4.0, "routed".to_string(), p),
            ]
        );
        let got: Vec<Uuid> = sqlx::query_scalar("SELECT order_id FROM order_state WHERE parent_order_id = $1 ORDER BY client_order_id")
            .bind(ids.parent_order_id)
            .fetch_all(&w.pool)
            .await
            .unwrap();
        assert_eq!(got, ids.child_order_ids.to_vec());
        // the parent is on the spread and is the only order sent
        let (inst, side, qty, limit): (String, String, f64, Option<f64>) = sqlx::query_as(
            "SELECT i.symbol, o.side, o.original_qty::float8, o.limit_price::float8 FROM order_state o \
             JOIN instrument i ON i.id = o.instrument_id::bigint WHERE o.order_id = $1",
        )
        .bind(ids.parent_order_id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
        assert_eq!((inst.as_str(), side.as_str(), qty, limit), ("ESZ6-ESH7", "buy", 4.0, Some(12.25)));
        assert_eq!(status(&w.pool, ids.parent_order_id).await, "routed");
        assert_eq!(*w.bag.sent.lock().unwrap(), vec![(ids.parent_order_id.to_string(), "buy".to_string(), 4.0)]);
        // the created event names the parent
        let named: Option<String> = sqlx::query_scalar(
            "SELECT payload_json->'payload'->>'parent_order_id' FROM order_event WHERE order_id = $1 AND event_type = 'order_submitted'",
        )
        .bind(ids.child_order_ids[0])
        .fetch_one(&w.pool)
        .await
        .unwrap();
        assert_eq!(named, Some(ids.parent_order_id.to_string()));
    }

    fn leg(native_id: &str, side: &str) -> BrokerLeg {
        BrokerLeg { native_id: native_id.into(), side: side.into(), ratio: 1 }
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn the_roll_goes_to_the_broker_with_its_directional_legs() {
        let w = world(4.0, false).await;
        submit_roll(&w.state, &w.me, roll(&w, 4.0)).await.unwrap();
        assert_eq!(*w.bag.legs.lock().unwrap(), vec![vec![leg("1001", "sell"), leg("1002", "buy")]]);
        let w = world(-2.0, false).await;
        submit_roll(&w.state, &w.me, roll(&w, 2.0)).await.unwrap();
        assert_eq!(*w.bag.legs.lock().unwrap(), vec![vec![leg("1001", "buy"), leg("1002", "sell")]]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn a_suspended_closing_order_uses_up_the_position() {
        let w = world(4.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 4.0)).await.unwrap();
        sqlx::query("UPDATE order_state SET status = 'suspended' WHERE order_id = $1")
            .bind(ids.child_order_ids[0])
            .execute(&w.pool)
            .await
            .unwrap();
        let err = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.err().unwrap();
        assert_eq!((err.status, err.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "roll quantity 1 exceeds the 0 ESZ6 not already being closed"));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn short_roll_reverses_the_children() {
        let w = world(-2.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 2.0)).await.unwrap();
        let kids: Vec<_> = children(&w.pool, ids.parent_order_id).await.into_iter().map(|(i, s, q, _, _)| (i, s, q)).collect();
        assert_eq!(kids, vec![(w.near, "buy".to_string(), 2.0), (w.far, "sell".to_string(), 2.0)]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn partial_qty_rolls_only_that_much() {
        let w = world(4.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap();
        let qtys: Vec<f64> = children(&w.pool, ids.parent_order_id).await.into_iter().map(|k| k.2).collect();
        assert_eq!(qtys, vec![1.0, 1.0]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn qty_above_position_is_refused() {
        let w = world(4.0, false).await;
        let e = submit_roll(&w.state, &w.me, roll(&w, 5.0)).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "roll quantity 5 exceeds the position of 4"));
        assert_eq!(order_count(&w.pool, w.pf).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rolling_the_full_position_twice_refuses_the_second() {
        let w = world(4.0, false).await;
        submit_roll(&w.state, &w.me, roll(&w, 4.0)).await.unwrap();
        let e = submit_roll(&w.state, &w.me, roll(&w, 4.0)).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "roll quantity 4 exceeds the 0 ESZ6 not already being closed"));
        // a roll whose broker call went unanswered still holds its quantity
        let w = world(4.0, true).await;
        submit_roll(&w.state, &w.me, roll(&w, 3.0)).await.unwrap_err();
        let e = submit_roll(&w.state, &w.me, roll(&w, 2.0)).await.unwrap_err();
        assert_eq!(e.message, "roll quantity 2 exceeds the 1 ESZ6 not already being closed");
        assert_eq!(order_count(&w.pool, w.pf).await, 3);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn a_limit_roll_may_have_a_negative_spread_price() {
        let w = world(-2.0, false).await;
        let mut r = roll(&w, 2.0);
        r.limit_price = Some(-1.25);
        let ids = submit_roll(&w.state, &w.me, r).await.unwrap();
        let px: Option<f64> = sqlx::query_scalar("SELECT limit_price::float8 FROM order_state WHERE order_id = $1")
            .bind(ids.parent_order_id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
        assert_eq!(px, Some(-1.25));
        assert_eq!(status(&w.pool, ids.parent_order_id).await, "routed");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn a_leg_below_the_broker_minimum_is_refused() {
        let w = world(4.0, false).await;
        sqlx::query("UPDATE broker_instrument SET min_quantity = 2 WHERE instrument_id = $1").bind(w.far).execute(&w.pool).await.unwrap();
        let e = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "quantity 1 below broker minimum 2 for ESH7"));
        assert_eq!(order_count(&w.pool, w.pf).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn the_order_views_name_a_legs_parent() {
        let w = world(4.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap();
        let axum::Json(leg) = crate::handlers::get_order(
            axum::extract::State(w.state.clone()),
            axum::extract::Extension(w.me.clone()),
            axum::extract::Path(ids.child_order_ids[0].to_string()),
        )
        .await
        .unwrap();
        assert_eq!(leg.parent_order_id, Some(ids.parent_order_id));
        let axum::Json(rows) = crate::handlers::list_orders(
            axum::extract::State(w.state.clone()),
            axum::extract::Extension(w.me.clone()),
            axum::extract::Query(serde_json::from_value(serde_json::json!({})).unwrap()),
        )
        .await
        .unwrap();
        let parents: Vec<Option<Uuid>> = rows.iter().filter(|r| r.order_id != ids.parent_order_id).map(|r| r.parent_order_id).collect();
        assert_eq!(parents, vec![Some(ids.parent_order_id); 2]);
        assert!(rows.iter().any(|r| r.order_id == ids.parent_order_id && r.parent_order_id.is_none()));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn roll_without_position_is_refused() {
        let w = world(0.0, false).await;
        let e = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "no position in ESZ6 to roll"));
        assert_eq!(order_count(&w.pool, w.pf).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn roll_into_another_root_or_an_earlier_contract_is_refused() {
        let w = world(4.0, false).await;
        let nq = future(&w.pool, "NQH7", "NQ", "2027-03-19", "2001").await;
        let mut r = roll(&w, 1.0);
        r.to_instrument_id = nq;
        let e = submit_roll(&w.state, &w.me, r).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "NQH7 is not a later ES contract than ESZ6"));
        let mut r = roll(&w, 1.0);
        r.to_instrument_id = w.near;
        let e = submit_roll(&w.state, &w.me, r).await.unwrap_err();
        assert_eq!(e.message, "ESZ6 is not a later ES contract than ESZ6");
        assert_eq!(order_count(&w.pool, w.pf).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn a_risk_reject_on_one_leg_rejects_the_roll() {
        let w = world(4.0, false).await;
        // one ESH7 contract is 6010 x 50 = 300,500 notional
        w.state.marks().set(w.near, 5999.0, 6001.0);
        w.state.marks().set(w.far, 6009.0, 6011.0);
        sqlx::query("INSERT INTO risk_limits (portfolio_id, instrument_id, max_order_notional) VALUES ($1, $2, 100000)")
            .bind(w.pf)
            .bind(w.far.to_string())
            .execute(&w.pool)
            .await
            .unwrap();
        let e = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap_err();
        assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(e.message.starts_with("risk check failed [max_order_notional]"), "{}", e.message);
        let all: Vec<(String, Option<Uuid>)> =
            sqlx::query_as("SELECT status, parent_order_id FROM order_state WHERE portfolio_id = $1").bind(w.pf).fetch_all(&w.pool).await.unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|(s, _)| s == "rejected"), "{all:?}");
        assert_eq!(all.iter().filter(|(_, p)| p.is_some()).count(), 2);
        assert!(w.bag.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn spread_instrument_is_created_once() {
        let w = world(4.0, false).await;
        submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap();
        submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap();
        let spreads: Vec<(i64, String, String, f64, f64)> = sqlx::query_as(
            "SELECT id, venue, instrument_class, contract_size::float8, price_increment::float8 FROM instrument WHERE symbol = 'ESZ6-ESH7'",
        )
        .fetch_all(&w.pool)
        .await
        .unwrap();
        assert_eq!(spreads.len(), 1);
        let (id, venue, class, size, tick) = spreads[0].clone();
        assert_eq!((venue.as_str(), class.as_str(), size, tick), ("XCME", "FUTURE", 50.0, 0.25));
        let legs: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT instrument_id, near_id, far_id FROM future_spread").fetch_all(&w.pool).await.unwrap();
        assert_eq!(legs, vec![(id, w.near, w.far)]);
        assert_eq!(ensure_spread(&w.pool, w.near, w.far).await.unwrap(), id);
        let unmapped: i64 = sqlx::query_scalar("SELECT count(*) FROM broker_instrument WHERE instrument_id = $1").bind(id).fetch_one(&w.pool).await.unwrap();
        assert_eq!(unmapped, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cancelling_a_child_is_refused() {
        let w = world(4.0, false).await;
        let ids = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap();
        let e = cancel(&w.state, &w.me, ids.child_order_ids[0], None).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "cancel the roll, not a leg"));
        assert_eq!(status(&w.pool, ids.child_order_ids[0]).await, "routed");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cancelling_an_unrouted_parent_cancels_its_children() {
        let w = world(4.0, true).await;
        // the broker call failed without an answer: all three stay submitted
        let e = submit_roll(&w.state, &w.me, roll(&w, 1.0)).await.unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        let parent: Uuid = sqlx::query_scalar("SELECT order_id FROM order_state WHERE portfolio_id = $1 AND parent_order_id IS NULL")
            .bind(w.pf)
            .fetch_one(&w.pool)
            .await
            .unwrap();
        assert_eq!(cancel(&w.state, &w.me, parent, Some("changed my mind".into())).await.unwrap(), CancelOutcome::Canceled);
        let all: Vec<String> = sqlx::query_scalar("SELECT status FROM order_state WHERE portfolio_id = $1").bind(w.pf).fetch_all(&w.pool).await.unwrap();
        assert_eq!(all, vec!["canceled"; 3]);
    }
}
