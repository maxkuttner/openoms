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
