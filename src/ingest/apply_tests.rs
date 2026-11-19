//! DB tests for `ingest`, each against its own freshly migrated database
//! (`OMS_RELOAD_TEST_DATABASE_URL`).

use gateway::adapter::{BrokerError, BrokerInstrument, InstrumentProvider};
use gateway::venue_event::{VenueEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};
use sqlx::PgPool;
use uuid::Uuid;

use super::ingest;
use crate::authz_schema_tests::migrated_db;
use crate::event_bus::EventBus;
use crate::ingest::decide::{BreakKind, Decision};

pub(crate) const CONN: &str = "BIN-TEST";

pub(crate) struct World {
    pub(crate) pool: PgPool,
    pub(crate) bus: EventBus,
    pub(crate) portfolio: Uuid,
    pub(crate) account: Uuid,
    pub(crate) instrument: i64,
}

pub(crate) async fn world() -> World {
    let pool = migrated_db().await;
    let portfolio = Uuid::new_v4();
    let account = Uuid::new_v4();
    let principal = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, 'PF', 'pf', 'ACTIVE')")
        .bind(portfolio).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO broker_connection (code, broker_code, environment, status, default_portfolio_id) \
         VALUES ($1, 'BINANCE', 'PAPER', 'ACTIVE', $2)",
    )
    .bind(CONN).bind(portfolio).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) \
         VALUES ($1, 'ACC', $2, 'acc', 'ACTIVE')",
    )
    .bind(account).bind(CONN).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, 'admin', 'HUMAN', 'ACTIVE')")
        .bind(principal).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, 'test')")
        .bind(principal).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO currency (code, name) VALUES ('USDT', 'Tether') ON CONFLICT DO NOTHING")
        .execute(&pool).await.unwrap();
    let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    let instrument: i64 = sqlx::query_scalar(
        "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, \
         price_precision, price_increment) \
         VALUES ('BTCUSDT', $1, 'BTC/USDT', 'CRYPTO', 'SPOT', 'USDT', 'ACTIVE', 2, 0.01) RETURNING id",
    )
    .bind(venue).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol) VALUES ($1, 'BINANCE', 'BTCUSDT')")
        .bind(instrument).execute(&pool).await.unwrap();
    World { pool, bus: EventBus::new(), portfolio, account, instrument }
}

fn fill(exec: &str, venue_order: &str, client: Option<&str>, qty: f64) -> VenueEvent {
    VenueEvent::Fill(VenueFill {
        exec_id: exec.into(),
        venue_order_id: venue_order.into(),
        client_order_id: client.map(str::to_string),
        account_ref: "acc".into(),
        symbol: "BTCUSDT".into(),
        buy: true,
        qty,
        price: 100.0,
        fee: None,
        executed_at: None,
    })
}

fn order(venue_order: &str, qty: f64, cum: f64, status: VenueOrderStatus) -> VenueEvent {
    VenueEvent::Order(VenueOrderUpdate {
        venue_order_id: venue_order.into(),
        client_order_id: None,
        account_ref: "acc".into(),
        symbol: "BTCUSDT".into(),
        buy: true,
        order_type: "LIMIT".into(),
        qty,
        limit_px: Some(100.0),
        status,
        cum_qty: cum,
        ts: None,
        created_at: None,
    })
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// (order_id, origin, portfolio, qty, cum, status) of the only order.
async fn only_order(pool: &PgPool) -> (Uuid, String, Uuid, f64, f64, String) {
    sqlx::query_as(
        "SELECT order_id, origin, portfolio_id, original_qty::float8, cum_qty::float8, status FROM order_state",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn position(w: &World) -> f64 {
    sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
        .bind(w.portfolio)
        .bind(w.instrument.to_string())
        .fetch_optional(&w.pool)
        .await
        .unwrap()
        .unwrap_or(0.0)
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn external_fill_adopts_into_default_book() {
    let w = world().await;
    let d = ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 2.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Adopt { portfolio_id: w.portfolio });
    let (_, origin, portfolio, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((origin.as_str(), portfolio, qty, cum, status.as_str()), ("EXTERNAL", w.portfolio, 2.0, 2.0, "filled"));
    let ext: String = sqlx::query_scalar("SELECT external_order_id FROM order_state").fetch_one(&w.pool).await.unwrap();
    assert_eq!(ext, "v1");
    assert_eq!(position(&w).await, 2.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
    let actor: String = sqlx::query_scalar("SELECT DISTINCT actor FROM order_event").fetch_one(&w.pool).await.unwrap();
    assert_eq!(actor, format!("ingest:{CONN}"));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn concurrent_stream_and_pull_book_once() {
    let w = world().await;
    let (a, b) = tokio::join!(
        ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 2.0), None),
        ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 2.0), None),
    );
    let mut ds = [a.unwrap().decision, b.unwrap().decision];
    ds.sort_by_key(|d| matches!(d, Decision::Duplicate));
    assert_eq!(ds[1], Decision::Duplicate);
    assert!(matches!(ds[0], Decision::Adopt { .. }));
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_state").await, 1);
    assert_eq!(only_order(&w.pool).await.4, 2.0);
    assert_eq!(position(&w).await, 2.0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn fill_first_then_update_grows_qty() {
    let w = world().await;
    ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 2.0), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 2.0, VenueOrderStatus::Working), None).await.unwrap().decision;
    assert!(matches!(d, Decision::Grow { new_qty, .. } if new_qty == 5.0));
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (5.0, 2.0, "partially_filled"));
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
    assert_eq!(position(&w).await, 2.0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn oms_order_fill_by_client_uuid_books() {
    let w = world().await;
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM principal").fetch_one(&w.pool).await.unwrap();
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, limit_price, original_qty, \
         leaves_qty, cum_qty, status, version, origin) \
         VALUES ($1, 'mine', $2, $3, $4, $5, $6, 'buy', 'limit', 'gtc', 100, 5, 5, 0, 'routed', 0, 'API')",
    )
    .bind(order_id).bind(principal).bind(w.portfolio).bind(w.account).bind(CONN).bind(w.instrument.to_string())
    .execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, CONN, fill("e1", "v-oms", Some(&order_id.to_string()), 2.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id });
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_state").await, 1);
    let (id, origin, _, _, cum, status) = only_order(&w.pool).await;
    assert_eq!((id, origin.as_str(), cum, status.as_str()), (order_id, "API", 2.0, "partially_filled"));
    assert_eq!(position(&w).await, 2.0);
}

/// An adopted order of 5 with one fill of 1 booked.
async fn order_with_one_fill(w: &World) {
    ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 0.0, VenueOrderStatus::Working), None).await.unwrap();
    ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 1.0), None).await.unwrap();
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn canceled_update_with_gap_closes_and_flags_gap_without_booking() {
    let w = world().await;
    order_with_one_fill(&w).await;
    let done = ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert!(done.fill_gap);
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (5.0, 1.0, "canceled"));
    assert_eq!(position(&w).await, 1.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn late_fill_on_closed_order_books_and_keeps_terminal_status() {
    let w = world().await;
    order_with_one_fill(&w).await;
    ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
    let done = ingest(&w.pool, &w.bus, CONN, fill("e2", "v1", None, 2.0), None).await.unwrap();
    assert!(matches!(done.decision, Decision::Apply { .. }));
    let (order_id, _, _, _, cum, status) = only_order(&w.pool).await;
    assert_eq!((cum, status.as_str()), (3.0, "canceled"));
    assert_eq!(position(&w).await, 3.0);
    let after: String = sqlx::query_scalar(
        "SELECT lower(payload_json->>'status_after') FROM order_event WHERE order_id = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(order_id).fetch_one(&w.pool).await.unwrap();
    assert_eq!(after, "canceled");
    // nothing left to pull
    let done = ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert!(!done.fill_gap);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn break_collects_later_events() {
    let w = world().await;
    sqlx::query("UPDATE broker_connection SET external_policy = 'break' WHERE code = $1")
        .bind(CONN).execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 1.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Break(BreakKind::PolicyBreak));
    let d = ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 1.0, VenueOrderStatus::Canceled), None).await.unwrap().decision;
    assert_eq!(d, Decision::AttachToBreak);
    // the same fill again adds nothing
    ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 1.0), None).await.unwrap();
    let (kind, n): (String, i32) = sqlx::query_as(
        "SELECT kind, jsonb_array_length(events_json) FROM recon_break WHERE status = 'open'",
    )
    .fetch_one(&w.pool).await.unwrap();
    assert_eq!((kind.as_str(), n), ("policy_break", 2));
    assert_eq!(count(&w.pool, "SELECT count(*) FROM recon_break").await, 1);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_state").await, 0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn late_fee_books_once() {
    let w = world().await;
    ingest(&w.pool, &w.bus, CONN, fill("e1", "v1", None, 1.0), None).await.unwrap();
    let fee = || VenueEvent::Fee { exec_id: "e1".into(), account_ref: "acc".into(), amount: 0.1, currency: "USDT".into() };
    ingest(&w.pool, &w.bus, CONN, fee(), None).await.unwrap();
    ingest(&w.pool, &w.bus, CONN, fee(), None).await.unwrap();
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    let fee_id: String = sqlx::query_scalar("SELECT fee_id FROM order_fee").fetch_one(&w.pool).await.unwrap();
    assert_eq!(fee_id, "BINANCE-e1");
}

/// A broker catalog that lists nothing.
struct EmptyListing;

#[async_trait::async_trait]
impl InstrumentProvider for EmptyListing {
    async fn list_instruments(&self, _: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
        Ok(Vec::<BrokerInstrument>::new())
    }
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn unlisted_symbol_is_unknown_instrument_break() {
    let w = world().await;
    let mut ev = fill("e1", "v1", None, 1.0);
    if let VenueEvent::Fill(f) = &mut ev {
        f.symbol = "ETHUSDT".into();
    }
    let d = ingest(&w.pool, &w.bus, CONN, ev, Some(&EmptyListing)).await.unwrap().decision;
    assert_eq!(d, Decision::Break(BreakKind::UnknownInstrument));
    let kind: String = sqlx::query_scalar("SELECT kind FROM recon_break WHERE status = 'open'")
        .fetch_one(&w.pool).await.unwrap();
    assert_eq!(kind, "unknown_instrument");
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_state").await, 0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn order_update_grow_on_canceled_order_is_noop() {
    let w = world().await;
    order_with_one_fill(&w).await;
    ingest(&w.pool, &w.bus, CONN, order("v1", 5.0, 1.0, VenueOrderStatus::Canceled), None).await.unwrap();
    let events = count(&w.pool, "SELECT count(*) FROM order_event").await;
    ingest(&w.pool, &w.bus, CONN, order("v1", 8.0, 1.0, VenueOrderStatus::Working), None).await.unwrap();
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (5.0, 1.0, "canceled"));
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_event").await, events);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn fee_on_redelivered_fill_is_booked_once_even_if_first_attempt_missed_it() {
    let w = world().await;
    let mut ev = fill("e1", "v1", None, 1.0);
    if let VenueEvent::Fill(f) = &mut ev {
        f.fee = Some((0.1, "USDT".into()));
    }
    ingest(&w.pool, &w.bus, CONN, ev.clone(), None).await.unwrap();
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    // the post-commit fee step was lost
    sqlx::query("DELETE FROM order_fee").execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, CONN, ev.clone(), None).await.unwrap().decision;
    assert_eq!(d, Decision::Duplicate);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    ingest(&w.pool, &w.bus, CONN, ev, None).await.unwrap();
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
}

#[test]
fn events_round_trip_through_a_break() {
    let at = Some(chrono::DateTime::parse_from_rfc3339("2026-11-18T10:00:00Z").unwrap().with_timezone(&chrono::Utc));
    let mut f = fill("e1", "v1", Some("c-1"), 1.5);
    if let VenueEvent::Fill(x) = &mut f {
        x.fee = Some((0.25, "USDT".into()));
        x.executed_at = at;
    }
    let mut o = order("v1", 5.0, 1.5, VenueOrderStatus::Rejected { reason: "nope".into() });
    if let VenueEvent::Order(x) = &mut o {
        x.ts = at;
    }
    let all = [
        f,
        o,
        order("v1", 5.0, 0.0, VenueOrderStatus::Working),
        order("v1", 5.0, 5.0, VenueOrderStatus::Filled),
        order("v1", 5.0, 0.0, VenueOrderStatus::Canceled),
        order("v1", 5.0, 0.0, VenueOrderStatus::Expired),
    ];
    for event in all {
        let json = super::event_json(&event);
        assert_eq!(super::event_from_json(&json), Some(event), "{json}");
    }
    assert_eq!(super::event_from_json(&serde_json::json!({"type": "mystery"})), None);
}
