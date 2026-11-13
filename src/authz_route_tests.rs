//! Role enforcement on the trading routes (orders, positions, submit, cancel).
//! DB tests: run with `--ignored` against a migrated database (see
//! `handlers::tests::test_pool`). Rows are left behind (order_event is
//! append-only), so every seeded code carries a fresh UUID.

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};
use crate::handlers::tests::{seed_instrument, seed_order_state, test_app_state, test_pool};
use crate::handlers::{self, BlotterFilter, SubmitOrderRequest};

/// An ACTIVE principal of `kind` holding `role`.
async fn user(pool: &PgPool, role: &str, kind: &str) -> AuthContext {
    let id = Uuid::new_v4();
    let code = format!("authz-route-{role}-{id}");
    sqlx::query(
        "INSERT INTO principal (id, code, principal_type, display_name, status) \
         VALUES ($1, $2, $3, $2, 'ACTIVE')",
    )
    .bind(id)
    .bind(&code)
    .bind(kind)
    .execute(pool)
    .await
    .expect("seed principal");
    sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
        .bind(id)
        .bind(role)
        .execute(pool)
        .await
        .expect("seed role");
    AuthContext {
        principal_id: id,
        principal_code: code,
    }
}

async fn grant(pool: &PgPool, principal_id: Uuid, portfolio_id: Uuid, can_trade: bool) {
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) \
         VALUES ($1, $2, $3, $4, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal_id)
    .bind(portfolio_id)
    .bind(can_trade)
    .execute(pool)
    .await
    .expect("seed grant");
}

/// An order by `owner` in a fresh portfolio; returns (order, portfolio).
async fn order(pool: &PgPool, owner: Uuid) -> (Uuid, Uuid) {
    let instrument = seed_instrument(pool, "AUTHZ").await;
    let order_id = seed_order_state(pool, owner, instrument, "authz-route").await;
    (order_id, portfolio_of(pool, order_id).await)
}

async fn order_in(pool: &PgPool, owner: Uuid, portfolio: Uuid) -> Uuid {
    let (order_id, _) = order(pool, owner).await;
    sqlx::query("UPDATE order_state SET portfolio_id = $2 WHERE order_id = $1")
        .bind(order_id)
        .bind(portfolio)
        .execute(pool)
        .await
        .expect("move order");
    order_id
}

async fn portfolio_of(pool: &PgPool, order_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT portfolio_id FROM order_state WHERE order_id = $1")
        .bind(order_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A portfolio whose default account routes to a broker no adapter serves, and
/// an instrument tradeable there: a permitted submit stores the order and then
/// fails routing with 503.
pub(crate) async fn tradeable_portfolio(pool: &PgPool) -> (Uuid, i64) {
    let s = Uuid::new_v4().simple().to_string();
    let instrument = seed_instrument(pool, "AUTHZSUB").await;
    let broker = format!("AUTHZ{s}");
    sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $1, 'PAPER', 'ACTIVE')")
        .bind(&broker)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, $3, true)")
        .bind(instrument)
        .bind(&broker)
        .bind(format!("SYM{s}"))
        .execute(pool)
        .await
        .unwrap();
    let account = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) \
         VALUES ($1, $2, $2, $2, 'ACTIVE')",
    )
    .bind(account)
    .bind(&broker)
    .execute(pool)
    .await
    .unwrap();
    let portfolio = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status, default_account_id) VALUES ($1, $2, $2, 'ACTIVE', $3)")
        .bind(portfolio)
        .bind(format!("authz-{s}"))
        .bind(account)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE account SET portfolio_id = $2 WHERE id = $1")
        .bind(account)
        .bind(portfolio)
        .execute(pool)
        .await
        .unwrap();
    (portfolio, instrument)
}

fn submit_request(portfolio: Uuid, instrument: i64) -> (Uuid, SubmitOrderRequest) {
    let order_id = Uuid::new_v4();
    let req = SubmitOrderRequest {
        order_id: order_id.to_string(),
        client_order_id: format!("authz-{order_id}"),
        portfolio_id: portfolio.to_string(),
        account_id: None,
        instrument_id: Some(instrument.to_string()),
        symbol: None,
        venue: None,
        side: OrderSide::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Day,
        limit_price: None,
        quantity: 1.0,
    };
    (order_id, req)
}

fn no_filter() -> BlotterFilter {
    BlotterFilter {
        status: None,
        portfolio_id: None,
        instrument_id: None,
        principal_id: None,
        broker_connection_code: None,
        side: None,
        since: None,
        until: None,
        limit: Some(500),
        offset: None,
        client_order_id: None,
    }
}

/// Whether `auth` may cancel `order_id`. The order is made terminal first, so
/// an authorized cancel stops at 422 ("already terminal") without touching the
/// event stream; a refused one is 403 before the order is read.
async fn may_cancel(pool: &PgPool, auth: &AuthContext, order_id: Uuid) -> Result<bool, StatusCode> {
    sqlx::query("UPDATE order_state SET status = 'filled' WHERE order_id = $1")
        .bind(order_id)
        .execute(pool)
        .await
        .unwrap();
    let state = test_app_state(pool.clone());
    match crate::order_entry::cancel(&state, auth, order_id, None).await {
        Err(e) if e.status == StatusCode::UNPROCESSABLE_ENTITY => Ok(true),
        Err(e) if e.status == StatusCode::FORBIDDEN => Ok(false),
        Err(e) => Err(e.status),
        Ok(_) => panic!("a terminal order cannot be cancelled"),
    }
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn trader_cannot_cancel_other_traders_order() {
    let pool = test_pool().await;
    let alice = user(&pool, "trader", "HUMAN").await;
    let bob = user(&pool, "trader", "HUMAN").await;
    let (order_id, portfolio) = order(&pool, alice.principal_id).await;
    grant(&pool, alice.principal_id, portfolio, true).await;
    grant(&pool, bob.principal_id, portfolio, true).await;

    assert_eq!(may_cancel(&pool, &bob, order_id).await, Ok(false));
    assert_eq!(
        may_cancel(&pool, &alice, order_id).await,
        Ok(true),
        "own order cancels"
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn pm_can_cancel_any_order_in_portfolio() {
    let pool = test_pool().await;
    let trader = user(&pool, "trader", "HUMAN").await;
    let pm = user(&pool, "pm", "HUMAN").await;
    let (order_id, portfolio) = order(&pool, trader.principal_id).await;
    grant(&pool, pm.principal_id, portfolio, true).await;

    assert_eq!(may_cancel(&pool, &pm, order_id).await, Ok(true));
}

async fn listed(pool: &PgPool, auth: &AuthContext) -> Vec<Uuid> {
    let state = test_app_state(pool.clone());
    let Json(rows) =
        handlers::list_orders(State(state), Extension(auth.clone()), Query(no_filter()))
            .await
            .expect("list orders");
    rows.into_iter().map(|r| r.order_id).collect()
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn trader_list_orders_shows_only_own() {
    let pool = test_pool().await;
    let me = user(&pool, "trader", "HUMAN").await;
    let other = user(&pool, "trader", "HUMAN").await;
    let (mine, portfolio) = order(&pool, me.principal_id).await;
    let theirs = order_in(&pool, other.principal_id, portfolio).await;
    grant(&pool, me.principal_id, portfolio, true).await;

    let seen = listed(&pool, &me).await;
    assert!(seen.contains(&mine));
    assert!(
        !seen.contains(&theirs),
        "another trader's order must not be listed"
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn viewer_list_orders_shows_all_in_portfolio() {
    let pool = test_pool().await;
    let viewer = user(&pool, "viewer", "HUMAN").await;
    let trader = user(&pool, "trader", "HUMAN").await;
    let (a, portfolio) = order(&pool, trader.principal_id).await;
    let b = order_in(&pool, trader.principal_id, portfolio).await;
    let (outside, _) = order(&pool, trader.principal_id).await;
    grant(&pool, viewer.principal_id, portfolio, false).await;

    let seen = listed(&pool, &viewer).await;
    assert!(seen.contains(&a) && seen.contains(&b));
    assert!(
        !seen.contains(&outside),
        "a portfolio without a grant stays hidden"
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn idor_get_order_outside_scope_is_403() {
    let pool = test_pool().await;
    let me = user(&pool, "trader", "HUMAN").await;
    let other = user(&pool, "trader", "HUMAN").await;
    // Another portfolio entirely, and another trader's order in my own one.
    let (foreign, _) = order(&pool, other.principal_id).await;
    let (mine, portfolio) = order(&pool, me.principal_id).await;
    let neighbour = order_in(&pool, other.principal_id, portfolio).await;
    grant(&pool, me.principal_id, portfolio, true).await;

    let get = |id: Uuid| {
        let state = test_app_state(pool.clone());
        let auth = me.clone();
        async move {
            handlers::get_order(State(state), Extension(auth), Path(id.to_string()))
                .await
                .map(|_| ())
                .map_err(|e| e.status)
        }
    };
    assert_eq!(get(foreign).await, Err(StatusCode::FORBIDDEN));
    assert_eq!(get(neighbour).await, Err(StatusCode::FORBIDDEN));
    assert_eq!(get(mine).await, Ok(()));
    assert_eq!(get(Uuid::new_v4()).await, Err(StatusCode::NOT_FOUND));

    let events = crate::order_events::get_order_events(
        State(test_app_state(pool.clone())),
        Extension(me.clone()),
        Path(neighbour.to_string()),
    )
    .await;
    assert_eq!(events.err().map(|e| e.status), Some(StatusCode::FORBIDDEN));

    let allocs = handlers::list_allocations(
        State(test_app_state(pool.clone())),
        Extension(me.clone()),
        Path(neighbour),
    )
    .await;
    assert_eq!(allocs.err().map(|e| e.status), Some(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn idor_positions_outside_scope_is_403() {
    let pool = test_pool().await;
    let me = user(&pool, "trader", "HUMAN").await;
    let nobody = user(&pool, "trader", "HUMAN").await;
    let (_, portfolio) = order(&pool, nobody.principal_id).await;
    let (_, mine) = order(&pool, me.principal_id).await;
    grant(&pool, me.principal_id, mine, false).await;

    let positions = |p: Uuid| {
        let state = test_app_state(pool.clone());
        let auth = me.clone();
        async move {
            handlers::get_portfolio_positions(State(state), Extension(auth), Path(p))
                .await
                .map(|_| ())
                .map_err(|e| e.status)
        }
    };
    assert_eq!(positions(portfolio).await, Err(StatusCode::FORBIDDEN));
    assert_eq!(positions(mine).await, Ok(()));

    // A role without positions.view is refused even with a grant.
    let roleless = user(&pool, "viewer", "HUMAN").await;
    sqlx::query("DELETE FROM principal_role WHERE principal_id = $1")
        .bind(roleless.principal_id)
        .execute(&pool)
        .await
        .unwrap();
    grant(&pool, roleless.principal_id, mine, true).await;
    let r = handlers::get_portfolio_positions(
        State(test_app_state(pool.clone())),
        Extension(roleless),
        Path(mine),
    )
    .await;
    assert_eq!(r.err().map(|e| e.status), Some(StatusCode::FORBIDDEN));
}

pub(crate) async fn submit(
    pool: &PgPool,
    auth: &AuthContext,
    portfolio: Uuid,
    instrument: i64,
) -> (Uuid, StatusCode) {
    let (order_id, req) = submit_request(portfolio, instrument);
    let state = test_app_state(pool.clone());
    let status =
        match handlers::orders_submit(State(state), Extension(auth.clone()), Json(req)).await {
            Ok(r) => r.status(),
            Err(e) => e.status,
        };
    (order_id, status)
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn idor_submit_to_ungranted_portfolio_is_403() {
    let pool = test_pool().await;
    let (portfolio, instrument) = tradeable_portfolio(&pool).await;

    let stranger = user(&pool, "trader", "HUMAN").await;
    assert_eq!(
        submit(&pool, &stranger, portfolio, instrument).await.1,
        StatusCode::FORBIDDEN
    );

    // In scope, but the role cannot create orders.
    let viewer = user(&pool, "viewer", "HUMAN").await;
    grant(&pool, viewer.principal_id, portfolio, true).await;
    assert_eq!(
        submit(&pool, &viewer, portfolio, instrument).await.1,
        StatusCode::FORBIDDEN
    );

    // The role can, but the grant is view-only.
    let view_only = user(&pool, "trader", "HUMAN").await;
    grant(&pool, view_only.principal_id, portfolio, false).await;
    assert_eq!(
        submit(&pool, &view_only, portfolio, instrument).await.1,
        StatusCode::FORBIDDEN
    );
}

/// A strategy with `owner` as its responsible user; returns the strategy's principal.
async fn strategy_of(pool: &PgPool, owner: Uuid) -> AuthContext {
    let service = user(pool, "strategy", "STRATEGY").await;
    let portfolio = Uuid::new_v4();
    let code = format!("authz-st-{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
        .bind(portfolio)
        .bind(&code)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, principal_id, sender_comp_id, target_comp_id) \
         VALUES ($1, $1, $2, $3, $4, $1, 'OMS')",
    )
    .bind(&code)
    .bind(portfolio)
    .bind(owner)
    .bind(service.principal_id)
    .execute(pool)
    .await
    .unwrap();
    service
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn strategy_api_key_can_still_submit_and_cancel() {
    let pool = test_pool().await;
    let owner = user(&pool, "trader", "HUMAN").await;
    let strategy = strategy_of(&pool, owner.principal_id).await;
    let (portfolio, instrument) = tradeable_portfolio(&pool).await;
    grant(&pool, strategy.principal_id, portfolio, true).await;

    let (_, status) = submit(&pool, &strategy, portfolio, instrument).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "authorized; only routing fails"
    );

    // cancel_any: someone else's order in the strategy's granted portfolio.
    let human = user(&pool, "trader", "HUMAN").await;
    let order_id = order_in(&pool, human.principal_id, portfolio).await;
    assert_eq!(may_cancel(&pool, &strategy, order_id).await, Ok(true));
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn submit_stamps_responsible_principal() {
    let pool = test_pool().await;
    let (portfolio, instrument) = tradeable_portfolio(&pool).await;

    let trader = user(&pool, "trader", "HUMAN").await;
    grant(&pool, trader.principal_id, portfolio, true).await;
    let owner = user(&pool, "trader", "HUMAN").await;
    let strategy = strategy_of(&pool, owner.principal_id).await;
    grant(&pool, strategy.principal_id, portfolio, true).await;

    let responsible = |order_id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT responsible_principal_id FROM order_state WHERE order_id = $1",
            )
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (by_trader, _) = submit(&pool, &trader, portfolio, instrument).await;
    assert_eq!(responsible(by_trader).await, Some(trader.principal_id));
    let (by_strategy, _) = submit(&pool, &strategy, portfolio, instrument).await;
    assert_eq!(responsible(by_strategy).await, Some(owner.principal_id));
}

async fn grant_allocate(pool: &PgPool, principal_id: Uuid, portfolio_id: Uuid, can_trade: bool) {
    sqlx::query(
        "INSERT INTO principal_portfolio_grant \
            (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
         VALUES ($1, $2, $3, $4, true, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal_id)
    .bind(portfolio_id)
    .bind(can_trade)
    .execute(pool)
    .await
    .expect("seed allocate grant");
}

/// A filled order by `owner` in a fresh portfolio; returns (order, portfolio).
async fn filled_order(pool: &PgPool, owner: Uuid) -> (Uuid, Uuid) {
    let (order_id, portfolio) = order(pool, owner).await;
    sqlx::query("UPDATE order_state SET cum_qty = 10, avg_px = 100 WHERE order_id = $1")
        .bind(order_id)
        .execute(pool)
        .await
        .expect("fill order");
    (order_id, portfolio)
}

async fn new_portfolio(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
        .bind(id)
        .bind(format!("authz-alloc-{id}"))
        .execute(pool)
        .await
        .expect("seed portfolio");
    id
}

async fn allocate(
    pool: &PgPool,
    auth: &AuthContext,
    order_id: Uuid,
    targets: &[Uuid],
) -> Result<usize, StatusCode> {
    let splits = targets
        .iter()
        .map(|p| handlers::AllocationSplit { portfolio_id: *p, qty: 1.0 })
        .collect();
    handlers::create_allocations(
        State(test_app_state(pool.clone())),
        Extension(auth.clone()),
        Path(order_id),
        Json(handlers::CreateAllocations { splits }),
    )
    .await
    .map(|Json(v)| v.len())
    .map_err(|e| e.status)
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn viewer_cannot_allocate() {
    let pool = test_pool().await;
    let owner = user(&pool, "trader", "HUMAN").await;
    let (order_id, source) = filled_order(&pool, owner.principal_id).await;
    let target = new_portfolio(&pool).await;
    // A stale can_allocate flag must not outrank the role.
    let viewer = user(&pool, "viewer", "HUMAN").await;
    grant_allocate(&pool, viewer.principal_id, source, true).await;
    grant(&pool, viewer.principal_id, target, false).await;
    assert_eq!(allocate(&pool, &viewer, order_id, &[target]).await, Err(StatusCode::FORBIDDEN));
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn allocate_to_unseen_target_portfolio_is_403() {
    let pool = test_pool().await;
    let trader = user(&pool, "trader", "HUMAN").await;
    let (order_id, source) = filled_order(&pool, trader.principal_id).await;
    grant_allocate(&pool, trader.principal_id, source, true).await;
    let seen = new_portfolio(&pool).await;
    grant(&pool, trader.principal_id, seen, false).await;
    let unseen = new_portfolio(&pool).await;
    assert_eq!(
        allocate(&pool, &trader, order_id, &[seen, unseen]).await,
        Err(StatusCode::FORBIDDEN)
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn trader_with_trade_grant_can_allocate_to_visible_targets() {
    let pool = test_pool().await;
    let trader = user(&pool, "trader", "HUMAN").await;
    let (order_id, source) = filled_order(&pool, trader.principal_id).await;
    grant_allocate(&pool, trader.principal_id, source, true).await;
    let a = new_portfolio(&pool).await;
    let b = new_portfolio(&pool).await;
    grant(&pool, trader.principal_id, a, false).await;
    grant(&pool, trader.principal_id, b, true).await;
    assert_eq!(allocate(&pool, &trader, order_id, &[a, b]).await, Ok(2));
}
