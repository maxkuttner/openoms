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

pub(crate) struct World {
    /// This world's own connection code: the live reconciler registry is
    /// process-global, so tests sharing one code pull each other's reconcilers.
    pub(crate) conn: String,
    pub(crate) pool: PgPool,
    pub(crate) bus: EventBus,
    pub(crate) portfolio: Uuid,
    pub(crate) account: Uuid,
    pub(crate) instrument: i64,
}

pub(crate) async fn world() -> World {
    let pool = migrated_db().await;
    let conn = format!("BIN-TEST-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let portfolio = Uuid::new_v4();
    let account = Uuid::new_v4();
    let principal = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, 'PF', 'pf', 'ACTIVE')")
        .bind(portfolio).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO broker_connection (code, broker_code, environment, status, default_portfolio_id) \
         VALUES ($1, 'BINANCE', 'PAPER', 'ACTIVE', $2)",
    )
    .bind(&conn).bind(portfolio).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) \
         VALUES ($1, 'ACC', $2, 'acc', 'ACTIVE')",
    )
    .bind(account).bind(&conn).execute(&pool).await.unwrap();
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
    World { conn, pool, bus: EventBus::new(), portfolio, account, instrument }
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
    let d = ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Adopt { portfolio_id: w.portfolio });
    let (_, origin, portfolio, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((origin.as_str(), portfolio, qty, cum, status.as_str()), ("EXTERNAL", w.portfolio, 2.0, 2.0, "filled"));
    let ext: String = sqlx::query_scalar("SELECT external_order_id FROM order_state").fetch_one(&w.pool).await.unwrap();
    assert_eq!(ext, "v1");
    assert_eq!(position(&w).await, 2.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
    let actor: String = sqlx::query_scalar("SELECT DISTINCT actor FROM order_event").fetch_one(&w.pool).await.unwrap();
    assert_eq!(actor, format!("ingest:{}", w.conn));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn concurrent_stream_and_pull_book_once() {
    let w = world().await;
    let (a, b) = tokio::join!(
        ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None),
        ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None),
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
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 2.0, VenueOrderStatus::Working), None).await.unwrap().decision;
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
    .bind(order_id).bind(principal).bind(w.portfolio).bind(w.account).bind(&w.conn).bind(w.instrument.to_string())
    .execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v-oms", Some(&order_id.to_string()), 2.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id });
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_state").await, 1);
    let (id, origin, _, _, cum, status) = only_order(&w.pool).await;
    assert_eq!((id, origin.as_str(), cum, status.as_str()), (order_id, "API", 2.0, "partially_filled"));
    assert_eq!(position(&w).await, 2.0);
}

/// An adopted order of 5 with one fill of 1 booked.
async fn order_with_one_fill(w: &World) {
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 0.0, VenueOrderStatus::Working), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 1.0), None).await.unwrap();
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn canceled_update_with_gap_closes_and_flags_gap_without_booking() {
    let w = world().await;
    order_with_one_fill(&w).await;
    let done = ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
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
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
    let done = ingest(&w.pool, &w.bus, &w.conn, fill("e2", "v1", None, 2.0), None).await.unwrap();
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
    let done = ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 3.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert!(!done.fill_gap);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn break_collects_later_events() {
    let w = world().await;
    sqlx::query("UPDATE broker_connection SET external_policy = 'break' WHERE code = $1")
        .bind(&w.conn).execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 1.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Break(BreakKind::PolicyBreak));
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 1.0, VenueOrderStatus::Canceled), None).await.unwrap().decision;
    assert_eq!(d, Decision::AttachToBreak);
    // the same fill again adds nothing
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 1.0), None).await.unwrap();
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
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 1.0), None).await.unwrap();
    let fee = || VenueEvent::Fee { exec_id: "e1".into(), account_ref: "acc".into(), amount: 0.1, currency: "USDT".into() };
    ingest(&w.pool, &w.bus, &w.conn, fee(), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fee(), None).await.unwrap();
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
    let d = ingest(&w.pool, &w.bus, &w.conn, ev, Some(&EmptyListing)).await.unwrap().decision;
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
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 1.0, VenueOrderStatus::Canceled), None).await.unwrap();
    let events = count(&w.pool, "SELECT count(*) FROM order_event").await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 8.0, 1.0, VenueOrderStatus::Working), None).await.unwrap();
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
    ingest(&w.pool, &w.bus, &w.conn, ev.clone(), None).await.unwrap();
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    // the post-commit fee step was lost
    sqlx::query("DELETE FROM order_fee").execute(&w.pool).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, ev.clone(), None).await.unwrap().decision;
    assert_eq!(d, Decision::Duplicate);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM order_fee").await, 1);
    ingest(&w.pool, &w.bus, &w.conn, ev, None).await.unwrap();
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
        x.created_at = at;
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

type Ts = chrono::DateTime<chrono::Utc>;

fn at(s: &str) -> Ts {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
}

fn timed_fill(exec: &str, venue_order: &str, qty: f64, executed_at: Option<&str>) -> VenueEvent {
    let mut ev = fill(exec, venue_order, None, qty);
    if let VenueEvent::Fill(f) = &mut ev {
        f.executed_at = executed_at.map(at);
    }
    ev
}

fn timed_order(
    venue_order: &str,
    qty: f64,
    cum: f64,
    status: VenueOrderStatus,
    ts: Option<&str>,
    created: Option<&str>,
) -> VenueEvent {
    let mut ev = order(venue_order, qty, cum, status);
    if let VenueEvent::Order(o) = &mut ev {
        o.ts = ts.map(at);
        o.created_at = created.map(at);
    }
    ev
}

/// (event_type, occurred_at, trade_time_estimated, status_after) of every event, in order.
async fn events(pool: &PgPool) -> Vec<(String, Option<Ts>, Option<bool>, String)> {
    sqlx::query_as(
        "SELECT event_type, occurred_at, (payload_json->>'trade_time_estimated')::bool, \
         lower(payload_json->>'status_after') FROM order_event ORDER BY global_position",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// (last_trade_at, venue_created_at, venue_cum_qty) of the order on venue order `v1`.
async fn order_times(pool: &PgPool) -> (Option<Ts>, Option<Ts>, Option<f64>) {
    sqlx::query_as(
        "SELECT last_trade_at, venue_created_at, venue_cum_qty::float8 FROM order_state WHERE external_order_id = 'v1'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn position_last_trade(w: &World) -> Option<Ts> {
    sqlx::query_scalar("SELECT last_trade_at FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
        .bind(w.portfolio)
        .bind(w.instrument.to_string())
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn fill_event_stores_venue_execution_time() {
    let w = world().await;
    let created = "2026-11-01T09:00:00Z";
    let t0 = "2026-11-01T09:00:01Z";
    let t1 = "2026-11-01T09:05:00Z";
    let t2 = "2026-11-01T09:06:00Z";
    let working = timed_order("v1", 5.0, 0.0, VenueOrderStatus::Working, Some(t0), Some(created));
    ingest(&w.pool, &w.bus, &w.conn, working, None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e1", "v1", 2.0, Some(t1)), None).await.unwrap();
    let canceled = timed_order("v1", 5.0, 2.0, VenueOrderStatus::Canceled, Some(t2), None);
    ingest(&w.pool, &w.bus, &w.conn, canceled, None).await.unwrap();
    assert_eq!(
        events(&w.pool).await,
        vec![
            ("order_observed".into(), Some(at(t0)), None, "routed".into()),
            ("order_partially_filled".into(), Some(at(t1)), None, "partially_filled".into()),
            ("order_canceled".into(), Some(at(t2)), None, "canceled".into()),
        ]
    );
    let (last, venue_created, _) = order_times(&w.pool).await;
    assert_eq!((last, venue_created), (Some(at(t1)), Some(at(created))));
    assert_eq!(position_last_trade(&w).await, Some(at(t1)));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn status_update_without_venue_time_has_null_occurred_at() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 0.0, VenueOrderStatus::Working), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 0.0, VenueOrderStatus::Canceled), None).await.unwrap();
    let ev = events(&w.pool).await;
    let got: Vec<(&str, Option<Ts>)> = ev.iter().map(|e| (e.0.as_str(), e.1)).collect();
    assert_eq!(got, vec![("order_observed", None), ("order_canceled", None)]);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn missing_execution_time_is_estimated_and_not_late() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e1", "v1", 1.0, Some("2026-11-01T09:05:00Z")), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e2", "v1", 1.0, None), None).await.unwrap();
    let (occurred, booked, estimated): (Option<Ts>, Ts, Option<bool>) = sqlx::query_as(
        "SELECT occurred_at, created_at, (payload_json->>'trade_time_estimated')::bool FROM order_event \
         WHERE payload_json->'payload'->>'execution_id' = 'e2'",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    let occurred = occurred.expect("an estimated fill still has an occurred_at");
    assert_eq!(estimated, Some(true));
    assert!((occurred - booked).num_seconds().abs() < 5, "{occurred} vs {booked}");
    // booking time is the latest trade time: not a late fill
    assert_eq!(order_times(&w.pool).await.0, Some(occurred));
    assert_eq!(position_last_trade(&w).await, Some(occurred));
    assert_eq!(position(&w).await, 2.0);
    let flagged = count(&w.pool, "SELECT count(*) FROM order_event WHERE payload_json ? 'trade_time_estimated'").await;
    assert_eq!(flagged, 1);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn observed_filled_order_then_fills_books_once() {
    let w = world().await;
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 10.0, 10.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert!(matches!(d.decision, Decision::Adopt { .. }));
    assert!(d.fill_gap);
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (10.0, 0.0, "filled"));
    assert_eq!(order_times(&w.pool).await.2, Some(10.0));
    assert_eq!(position(&w).await, 0.0);
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 4.0), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fill("e2", "v1", None, 6.0), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 4.0), None).await.unwrap();
    assert_eq!(d.decision, Decision::Duplicate);
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (10.0, 10.0, "filled"));
    assert_eq!(position(&w).await, 10.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 2);
    let types: Vec<String> = events(&w.pool).await.into_iter().map(|e| e.0).collect();
    assert_eq!(types, ["order_observed", "order_partially_filled", "order_filled"]);
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 10.0, 10.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert!(!d.fill_gap);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn late_partial_fill_on_observed_filled_order_keeps_filled_status() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 5.0, VenueOrderStatus::Filled), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None).await.unwrap();
    let (_, _, _, _, cum, status) = only_order(&w.pool).await;
    assert_eq!((cum, status.as_str()), (2.0, "filled"));
    let ev = events(&w.pool).await;
    assert_eq!((ev[1].0.as_str(), ev[1].3.as_str()), ("order_partially_filled", "filled"));
    assert_eq!(position(&w).await, 2.0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn observed_filled_order_grown_by_update_stays_filled() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 10.0, 10.0, VenueOrderStatus::Filled), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 12.0, 12.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert!(matches!(d.decision, Decision::Grow { new_qty, .. } if new_qty == 12.0));
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (12.0, 0.0, "filled"));
    assert!(events(&w.pool).await.iter().all(|e| e.3 == "filled"));
    assert_eq!(order_times(&w.pool).await.2, Some(12.0));
    // a stale update never lowers the venue's cum
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 12.0, 5.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.2, Some(12.0));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn observed_working_order_grown_by_canceled_update_closes_canceled() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 0.0, VenueOrderStatus::Working), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 8.0, 0.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert!(matches!(d.decision, Decision::Grow { new_qty, .. } if new_qty == 8.0));
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (8.0, 0.0, "canceled"));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn observed_filled_order_grown_by_canceled_update_closes_canceled() {
    let w = world().await;
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 5.0, VenueOrderStatus::Filled), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None).await.unwrap();
    let d = ingest(&w.pool, &w.bus, &w.conn, order("v1", 8.0, 2.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert!(matches!(d.decision, Decision::Grow { new_qty, .. } if new_qty == 8.0));
    let (_, _, _, qty, cum, status) = only_order(&w.pool).await;
    assert_eq!((qty, cum, status.as_str()), (8.0, 2.0, "canceled"));
    assert_eq!(position(&w).await, 2.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 1);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn venue_created_at_is_set_once() {
    let w = world().await;
    let first = "2026-11-01T09:00:00Z";
    // a first sighting without a create time (FIX), then one with it (REST)
    ingest(&w.pool, &w.bus, &w.conn, timed_order("v1", 5.0, 0.0, VenueOrderStatus::Working, None, None), None)
        .await
        .unwrap();
    assert_eq!(order_times(&w.pool).await.1, None);
    let with_time = timed_order("v1", 5.0, 1.0, VenueOrderStatus::Working, None, Some(first));
    ingest(&w.pool, &w.bus, &w.conn, with_time, None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.1, Some(at(first)));
    assert_eq!(order_times(&w.pool).await.2, Some(1.0));
    // a different create time later never overwrites it
    let other = timed_order("v1", 5.0, 2.0, VenueOrderStatus::Working, None, Some("2026-11-02T00:00:00Z"));
    ingest(&w.pool, &w.bus, &w.conn, other, None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.1, Some(at(first)));
    assert_eq!(order_times(&w.pool).await.2, Some(2.0));
    // known from the first sighting, then a different one with a status change
    let v2 = timed_order("v2", 5.0, 0.0, VenueOrderStatus::Working, None, Some(first));
    ingest(&w.pool, &w.bus, &w.conn, v2, None).await.unwrap();
    let v2 = timed_order("v2", 5.0, 0.0, VenueOrderStatus::Canceled, None, Some("2026-11-03T00:00:00Z"));
    ingest(&w.pool, &w.bus, &w.conn, v2, None).await.unwrap();
    let (created, status): (Option<Ts>, String) =
        sqlx::query_as("SELECT venue_created_at, status FROM order_state WHERE external_order_id = 'v2'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((created, status.as_str()), (Some(at(first)), "canceled"));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn last_trade_at_tracks_latest_fill() {
    let w = world().await;
    let (early, late, later) = ("2026-11-01T09:00:00Z", "2026-11-01T10:00:00Z", "2026-11-01T11:00:00Z");
    ingest(&w.pool, &w.bus, &w.conn, order("v1", 5.0, 0.0, VenueOrderStatus::Working), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e1", "v1", 1.0, Some(late)), None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.0, Some(at(late)));
    // an older fill arriving later does not move it back
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e2", "v1", 1.0, Some(early)), None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.0, Some(at(late)));
    assert_eq!(position_last_trade(&w).await, Some(at(late)));
    ingest(&w.pool, &w.bus, &w.conn, timed_fill("e3", "v1", 1.0, Some(later)), None).await.unwrap();
    assert_eq!(order_times(&w.pool).await.0, Some(at(later)));
    assert_eq!(position_last_trade(&w).await, Some(at(later)));
    assert_eq!(position(&w).await, 3.0);
}

/// A second portfolio's hidden account on the world's connection, same ref.
pub(crate) async fn shared_ref_account(w: &World, code: &str, status: &str) -> Uuid {
    let pf = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
        .bind(pf).bind(code).execute(&w.pool).await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $3, 'acc', $4, $5)",
    )
    .bind(id).bind(code).bind(&w.conn).bind(status).bind(pf).execute(&w.pool).await.unwrap();
    id
}

/// Two portfolios granted one connection share its ref: the oldest account
/// wins every time, even once it is closed (recon cursors hang on it).
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn resolve_account_is_deterministic_with_shared_ref() {
    let w = world().await;
    let _p2 = shared_ref_account(&w, "P2", "ACTIVE").await;
    let _p3 = shared_ref_account(&w, "P3", "ACTIVE").await;
    sqlx::query("UPDATE account SET status = 'CLOSED' WHERE id = $1").bind(w.account).execute(&w.pool).await.unwrap();
    for _ in 0..3 {
        assert_eq!(super::resolve_account(&w.pool, &w.conn, "acc").await.unwrap(), w.account);
    }
    // One distinct ref across several rows still serves as the single-account fallback.
    assert_eq!(super::resolve_account(&w.pool, &w.conn, "other").await.unwrap(), w.account);
}

/// A rule on a sibling account of the shared ref still attributes.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn rule_on_non_representative_shared_account_attributes() {
    let w = world().await;
    let sib = shared_ref_account(&w, "P2", "ACTIVE").await;
    let sib_pf: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM account WHERE id = $1").bind(sib).fetch_one(&w.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO attribution_rule (id, broker_connection_code, account_id, portfolio_id, priority) VALUES ($1, $2, $3, $4, 1)",
    )
    .bind(Uuid::new_v4()).bind(&w.conn).bind(sib).bind(sib_pf).execute(&w.pool).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, fill("e1", "v1", None, 2.0), None).await.unwrap();
    let (_, _, portfolio, _, _, _) = only_order(&w.pool).await;
    assert_eq!(portfolio, sib_pf);
}

/// A futures roll as Task 10 stores it, on an IBKR_TWS connection: the parent
/// (BUY 4 of the spread, routed as IBKR order 100) and its two leg children
/// (SELL 4 ESZ6, BUY 4 ESH7, routed, no venue id), over a long 4 ESZ6.
struct Roll {
    parent: Uuid,
    near_child: Uuid,
    far_child: Uuid,
    near: i64,
    far: i64,
    spread: i64,
}

async fn future(pool: &PgPool, symbol: &str) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, contract_size, status, \
         price_precision, price_increment) \
         VALUES ($1, 'XCME', $1, 'INDEX', 'FUTURE', 'USD', 50, 'ACTIVE', 2, 0.25) RETURNING id",
    )
    .bind(symbol).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, native_id) VALUES ($1, 'IBKR_TWS', $2, $3)")
        .bind(id).bind(symbol).bind(id.to_string()).execute(pool).await.unwrap();
    id
}

pub(crate) async fn insert_order(
    w: &World,
    order_id: Uuid,
    client: &str,
    instrument: i64,
    side: &str,
    external: Option<&str>,
    parent: Option<Uuid>,
) {
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM principal").fetch_one(&w.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, limit_price, original_qty, \
         leaves_qty, cum_qty, status, version, origin, external_order_id, parent_order_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'market', 'day', NULL, 4, 4, 0, 'routed', 0, 'API', $9, $10)",
    )
    .bind(order_id).bind(client).bind(principal).bind(w.portfolio).bind(w.account).bind(&w.conn)
    .bind(instrument.to_string()).bind(side).bind(external).bind(parent)
    .execute(&w.pool).await.unwrap();
}

async fn roll_world(w: &World) -> Roll {
    sqlx::query("UPDATE broker_connection SET broker_code = 'IBKR_TWS' WHERE code = $1")
        .bind(&w.conn).execute(&w.pool).await.unwrap();
    let near = future(&w.pool, "ESZ6").await;
    let far = future(&w.pool, "ESH7").await;
    let spread: i64 = sqlx::query_scalar(
        "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, contract_size, status, \
         price_precision, price_increment) \
         VALUES ('ESZ6-ESH7', 'XCME', 'ESZ6-ESH7', 'INDEX', 'FUTURE', 'USD', 50, 'ACTIVE', 2, 0.25) RETURNING id",
    )
    .fetch_one(&w.pool).await.unwrap();
    let (parent, near_child, far_child) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    insert_order(w, parent, &parent.to_string(), spread, "buy", Some("100"), None).await;
    insert_order(w, near_child, &format!("{parent}-1"), near, "sell", None, Some(parent)).await;
    insert_order(w, far_child, &format!("{parent}-2"), far, "buy", None, Some(parent)).await;
    sqlx::query("INSERT INTO position (portfolio_id, instrument_id, net_qty) VALUES ($1, $2, 4)")
        .bind(w.portfolio).bind(near.to_string()).execute(&w.pool).await.unwrap();
    Roll { parent, near_child, far_child, near, far, spread }
}

/// One leg execution of the roll's BAG: IBKR names the leg by its local symbol
/// and the order by the BAG's permId and order ref (the parent id).
fn leg_fill(r: &Roll, exec: &str, symbol: &str, buy: bool, qty: f64, price: f64) -> VenueEvent {
    VenueEvent::Fill(VenueFill {
        exec_id: exec.into(),
        venue_order_id: "P1".into(),
        client_order_id: Some(r.parent.to_string()),
        account_ref: "acc".into(),
        symbol: symbol.into(),
        buy,
        qty,
        price,
        fee: None,
        executed_at: Some(chrono::Utc::now()),
    })
}

/// IBKR's report on the BAG order itself (spread units).
fn bag_update(r: &Roll, cum: f64, status: VenueOrderStatus) -> VenueEvent {
    VenueEvent::Order(VenueOrderUpdate {
        venue_order_id: "P1".into(),
        client_order_id: Some(r.parent.to_string()),
        account_ref: "acc".into(),
        symbol: "ES".into(),
        buy: true,
        order_type: "limit".into(),
        qty: 4.0,
        limit_px: Some(40.0),
        status,
        cum_qty: cum,
        ts: None,
        created_at: None,
    })
}

async fn position_of(w: &World, instrument: i64) -> f64 {
    sqlx::query_scalar("SELECT net_qty::float8 FROM position WHERE portfolio_id = $1 AND instrument_id = $2")
        .bind(w.portfolio)
        .bind(instrument.to_string())
        .fetch_optional(&w.pool)
        .await
        .unwrap()
        .unwrap_or(0.0)
}

/// (status, cum) of one order.
async fn state_of(w: &World, order_id: Uuid) -> (String, f64) {
    sqlx::query_as("SELECT status, cum_qty::float8 FROM order_state WHERE order_id = $1")
        .bind(order_id).fetch_one(&w.pool).await.unwrap()
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn leg_fills_land_on_their_children() {
    let w = world().await;
    let r = roll_world(&w).await;
    let d = ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id: r.near_child });
    let d = ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x2", "ESH7", true, 4.0, 6040.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id: r.far_child });
    assert_eq!(position_of(&w, r.near).await, 0.0);
    assert_eq!(position_of(&w, r.far).await, 4.0);
    assert_eq!(state_of(&w, r.near_child).await, ("filled".to_string(), 4.0));
    assert_eq!(state_of(&w, r.far_child).await, ("filled".to_string(), 4.0));
    // the parent books no fills: its progress follows the legs, as
    // status-only events
    assert_eq!(state_of(&w, r.parent).await, ("filled".to_string(), 4.0));
    let kinds: Vec<String> = sqlx::query_scalar("SELECT event_type FROM order_event WHERE order_id = $1 ORDER BY version")
        .bind(r.parent).fetch_all(&w.pool).await.unwrap();
    // one leg alone moves nothing; the second completes the spread
    assert_eq!(kinds, ["order_legs_filled"]);
    let booked_to: Vec<Uuid> = sqlx::query_scalar("SELECT order_id FROM venue_fill ORDER BY exec_id").fetch_all(&w.pool).await.unwrap();
    assert_eq!(booked_to, vec![r.near_child, r.far_child]);
    // a redelivered leg fill is a duplicate
    let d = ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0), None).await.unwrap().decision;
    assert_eq!(d, Decision::Duplicate);
    assert_eq!(position_of(&w, r.near).await, 0.0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_leg_fill_matching_no_child_is_an_unknown_instrument_break() {
    let w = world().await;
    let r = roll_world(&w).await;
    // another contract, and the right contract on the wrong side
    for (exec, symbol, buy) in [("x1", "ESM7", false), ("x2", "ESZ6", true)] {
        let d = ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, exec, symbol, buy, 4.0, 6000.0), None).await.unwrap().decision;
        assert!(matches!(d, Decision::Break(BreakKind::UnknownInstrument) | Decision::AttachToBreak), "{d:?}");
    }
    let kind: String = sqlx::query_scalar("SELECT kind FROM recon_break").fetch_one(&w.pool).await.unwrap();
    assert_eq!(kind, "unknown_instrument");
    for id in [r.parent, r.near_child, r.far_child] {
        assert_eq!(state_of(&w, id).await, ("routed".to_string(), 0.0));
    }
    assert_eq!(position_of(&w, r.near).await, 4.0);
    assert_eq!(count(&w.pool, "SELECT count(*) FROM venue_fill").await, 0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn parent_status_update_is_not_a_fill_gap() {
    let w = world().await;
    let r = roll_world(&w).await;
    let done = ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 2.0, VenueOrderStatus::Working), None).await.unwrap();
    assert_eq!(done.decision, Decision::Apply { order_id: r.parent });
    assert!(!done.fill_gap, "a parent never books fills: nothing to pull for it");
    let done = ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 4.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert!(!done.fill_gap);
    // the venue's spread cum is on record; no fill was booked on the parent
    let (cum, venue_cum): (f64, Option<f64>) =
        sqlx::query_as("SELECT cum_qty::float8, venue_cum_qty::float8 FROM order_state WHERE order_id = $1")
            .bind(r.parent).fetch_one(&w.pool).await.unwrap();
    assert_eq!((cum, venue_cum), (0.0, Some(4.0)));
    // and the parent's fill never forces the children filled
    assert_eq!(state_of(&w, r.near_child).await, ("routed".to_string(), 0.0));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn cancelled_parent_cancels_unfilled_children() {
    let w = world().await;
    let r = roll_world(&w).await;
    // the near leg filled, the far leg partly (a broken fill, for the test's sake)
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x2", "ESH7", true, 1.0, 6040.0), None).await.unwrap();
    let done = ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 1.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert_eq!(done.decision, Decision::Apply { order_id: r.parent });
    assert_eq!(state_of(&w, r.parent).await, ("canceled".to_string(), 1.0));
    assert_eq!(state_of(&w, r.near_child).await, ("filled".to_string(), 4.0));
    assert_eq!(state_of(&w, r.far_child).await, ("canceled".to_string(), 1.0));
    let leaves: f64 = sqlx::query_scalar("SELECT leaves_qty::float8 FROM order_state WHERE order_id = $1")
        .bind(r.far_child).fetch_one(&w.pool).await.unwrap();
    assert_eq!(leaves, 0.0);
    let last: String = sqlx::query_scalar("SELECT event_type FROM order_event WHERE order_id = $1 ORDER BY version DESC LIMIT 1")
        .bind(r.far_child).fetch_one(&w.pool).await.unwrap();
    assert_eq!(last, "order_canceled");
    // a repeated cancel report changes nothing
    ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 1.0, VenueOrderStatus::Canceled), None).await.unwrap();
    assert_eq!(count(&w.pool, &format!("SELECT count(*) FROM order_event WHERE order_id = '{}'", r.far_child)).await, 2);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn rejected_or_expired_parent_closes_its_children_the_same_way() {
    for (status, want) in [
        (VenueOrderStatus::Rejected { reason: "IBKR order inactive".into() }, "rejected"),
        (VenueOrderStatus::Expired, "expired"),
    ] {
        let w = world().await;
        let r = roll_world(&w).await;
        ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 0.0, status), None).await.unwrap();
        for id in [r.parent, r.near_child, r.far_child] {
            assert_eq!(state_of(&w, id).await, (want.to_string(), 0.0), "{want}");
        }
    }
}

/// (status, cum, avg_px) of the parent.
async fn parent_progress(w: &World, r: &Roll) -> (String, f64, Option<f64>) {
    sqlx::query_as("SELECT status, cum_qty::float8, avg_px::float8 FROM order_state WHERE order_id = $1")
        .bind(r.parent).fetch_one(&w.pool).await.unwrap()
}

async fn spread_positions(w: &World, r: &Roll) -> i64 {
    count(&w.pool, &format!("SELECT count(*) FROM position WHERE instrument_id = '{}'", r.spread)).await
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn both_legs_filled_fill_the_parent_at_the_net_price() {
    let w = world().await;
    let r = roll_world(&w).await;
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0), None).await.unwrap();
    // one leg alone fills nothing of the spread
    assert_eq!(parent_progress(&w, &r).await, ("routed".to_string(), 0.0, None));
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x2", "ESH7", true, 2.0, 6040.0), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x3", "ESH7", true, 2.0, 6042.0), None).await.unwrap();
    // net debit: buy-leg avg 6041 - sell-leg avg 6000
    assert_eq!(parent_progress(&w, &r).await, ("filled".to_string(), 4.0, Some(41.0)));
    let leaves: f64 = sqlx::query_scalar("SELECT leaves_qty::float8 FROM order_state WHERE order_id = $1")
        .bind(r.parent).fetch_one(&w.pool).await.unwrap();
    assert_eq!(leaves, 0.0);
    assert_eq!(spread_positions(&w, &r).await, 0, "the parent never holds a position");
    // and positions rebuilt from the log agree: the legs only
    let fills: i64 = count(&w.pool, &format!(
        "SELECT count(*) FROM order_event WHERE order_id = '{}' AND event_type IN ('order_filled', 'order_partially_filled')", r.parent)).await;
    assert_eq!(fills, 0);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn one_leg_filled_the_other_partly_leaves_the_parent_partially_filled() {
    let w = world().await;
    let r = roll_world(&w).await;
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0), None).await.unwrap();
    ingest(&w.pool, &w.bus, &w.conn, leg_fill(&r, "x2", "ESH7", true, 1.0, 6040.0), None).await.unwrap();
    assert_eq!(parent_progress(&w, &r).await, ("partially_filled".to_string(), 1.0, Some(40.0)));
    assert_eq!(spread_positions(&w, &r).await, 0);
    // a venue Filled on the BAG moves nothing beyond its venue cum
    ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 4.0, VenueOrderStatus::Filled), None).await.unwrap();
    assert_eq!(parent_progress(&w, &r).await, ("partially_filled".to_string(), 1.0, Some(40.0)));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_leg_fill_found_by_the_parents_venue_order_id_lands_on_its_child() {
    let w = world().await;
    let r = roll_world(&w).await;
    let VenueEvent::Fill(mut f) = leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0) else { unreachable!() };
    f.client_order_id = None;
    f.venue_order_id = "100".into();
    let d = ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Fill(f), None).await.unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id: r.near_child });
    assert_eq!(state_of(&w, r.near_child).await, ("filled".to_string(), 4.0));
    assert_eq!(position_of(&w, r.near).await, 0.0);
    assert_eq!(state_of(&w, r.parent).await, ("routed".to_string(), 0.0));
}

/// Leg fills take the parent's row lock before the leg's: while another
/// transaction holds the parent (a cancel cascading to its legs), a leg fill
/// waits without having locked its leg, so the two can never deadlock.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_leg_fill_locks_the_parent_before_its_leg() {
    let w = world().await;
    let r = roll_world(&w).await;
    let mut holder = w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM order_state WHERE order_id = $1 FOR UPDATE")
        .bind(r.parent).execute(&mut *holder).await.unwrap();
    let (pool, bus, conn, fill) = (w.pool.clone(), w.bus.clone(), w.conn.clone(), leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0));
    let task = tokio::spawn(async move { ingest(&pool, &bus, &conn, fill, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the leg fill waits for the parent");
    // ...holding no lock on its leg meanwhile
    let mut probe = w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM order_state WHERE order_id = $1 FOR UPDATE NOWAIT")
        .bind(r.near_child).execute(&mut *probe).await.expect("leg row must be unlocked while the parent is held");
    probe.rollback().await.unwrap();
    holder.commit().await.unwrap();
    let d = task.await.unwrap().unwrap().decision;
    assert_eq!(d, Decision::Apply { order_id: r.near_child });
    assert_eq!(state_of(&w, r.near_child).await, ("filled".to_string(), 4.0));
}

/// A leg fill (found by the BAG's order id) racing the BAG's cancel (found by
/// its permId, so under a different venue-order lock) both complete.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_leg_fill_racing_a_parent_cancel_completes() {
    for _ in 0..5 {
        let w = world().await;
        let r = roll_world(&w).await;
        let VenueEvent::Fill(mut f) = leg_fill(&r, "x1", "ESZ6", false, 4.0, 6000.0) else { unreachable!() };
        f.client_order_id = None;
        f.venue_order_id = "100".into();
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(
                ingest(&w.pool, &w.bus, &w.conn, VenueEvent::Fill(f), None),
                ingest(&w.pool, &w.bus, &w.conn, bag_update(&r, 0.0, VenueOrderStatus::Canceled), None),
            )
        })
        .await
        .expect("no deadlock");
        a.unwrap();
        b.unwrap();
        assert_eq!(state_of(&w, r.parent).await.0, "canceled");
        assert_eq!(state_of(&w, r.far_child).await, ("canceled".to_string(), 0.0));
        let near = state_of(&w, r.near_child).await.0;
        assert!(near == "filled" || near == "canceled", "{near}");
    }
}
