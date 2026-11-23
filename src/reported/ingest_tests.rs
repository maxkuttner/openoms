//! DB tests for reported fill ingest (Task 4): booking, idempotency, breaks,
//! fees, sequence gaps, and the HTTP gate (token, path, schema, size).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::api;
use super::ingest::{ingest, BatchResult, FillBatch};
use crate::authz_schema_tests::migrated_db;

struct Fixture {
    pool: PgPool,
    code: String,
    portfolio: Uuid,
    instrument: i64,
}

/// A migrated DB with BTCUSDT (quote USDT, base BTC) listed on BINANCE and
/// BYBIT, and one REPORTED strategy with its own portfolio.
async fn fixture() -> Fixture {
    let pool = migrated_db().await;
    for sql in [
        "INSERT INTO venue (code, name) VALUES ('CRYPTO', 'c') ON CONFLICT DO NOTHING",
        "INSERT INTO currency (code, name) VALUES ('USDT', 'tether') ON CONFLICT DO NOTHING",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    let instrument: i64 = sqlx::query_scalar(
        "INSERT INTO instrument (symbol, name, venue, asset_class, instrument_class, currency, \
         price_precision, price_increment) VALUES ('BTCUSDT', 'btc', 'CRYPTO', 'CRYPTO', 'SPOT', 'USDT', 2, 0.01) \
         RETURNING id",
    )
    .fetch_one(&pool).await.unwrap();
    for broker in ["BINANCE", "BYBIT"] {
        sqlx::query(
            "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, native_id) \
             VALUES ($1, $2, 'BTCUSDT', 'BTC')",
        )
        .bind(instrument).bind(broker).execute(&pool).await.unwrap();
    }
    let code = "rep".to_string();
    let portfolio = new_strategy(&pool, &code).await;
    Fixture { pool, code, portfolio, instrument }
}

async fn new_strategy(pool: &PgPool, code: &str) -> Uuid {
    let owner = Uuid::new_v4();
    let portfolio = Uuid::new_v4();
    sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
        .bind(owner).bind(format!("own-{owner}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
        .bind(portfolio).bind(format!("RPT-{code}")).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, $1, $2, $3, 'REPORTED')",
    )
    .bind(code).bind(portfolio).bind(owner).execute(pool).await.unwrap();
    portfolio
}

fn fill(venue: &str, exec: &str, side: &str, qty: f64, price: f64, at: &str) -> Value {
    json!({
        "venue": venue, "account_ref": "main", "exec_id": exec, "symbol": "BTCUSDT",
        "side": side, "qty": qty, "price": price, "trade_time": at,
    })
}

fn batch(id: &str, sequence: Option<i64>, fills: Vec<Value>) -> FillBatch {
    FillBatch { schema_version: 1, batch_id: id.into(), sequence, fills }
}

fn outcomes(r: &BatchResult) -> Vec<&str> {
    r.results.iter().map(|o| o.outcome.as_str()).collect()
}

async fn position(f: &Fixture) -> (f64, f64, f64) {
    sqlx::query_as::<_, (f64, f64, f64)>(
        "SELECT net_qty::float8, avg_cost::float8, realized_pnl::float8 FROM position \
         WHERE portfolio_id = $1 AND instrument_id = $2",
    )
    .bind(f.portfolio).bind(f.instrument.to_string())
    .fetch_optional(&f.pool).await.unwrap()
    .unwrap_or((0.0, 0.0, 0.0))
}

fn close(got: (f64, f64, f64), want: (f64, f64, f64)) {
    let ok = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(ok(got.0, want.0) && ok(got.1, want.1) && ok(got.2, want.2), "got {got:?}, want {want:?}");
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn fill_books_into_strategy_portfolio() {
    let f = fixture().await;
    let r = ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BINANCE", "e1", "BUY", 0.01, 61000.5, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    assert_eq!(outcomes(&r), ["booked"]);
    close(position(&f).await, (0.01, 61000.5, 0.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn same_batch_twice_books_once() {
    let f = fixture().await;
    let b = || batch("b1", None, vec![fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z")]);
    let first = ingest(&f.pool, &f.code, b()).await.unwrap();
    let second = ingest(&f.pool, &f.code, b()).await.unwrap();
    assert_eq!(outcomes(&first), ["booked"]);
    assert_eq!(outcomes(&second), ["booked"], "replay returns the stored result");
    close(position(&f).await, (1.0, 100.0, 0.0));
    assert_eq!(count(&f.pool, "SELECT count(*) FROM reported_fill").await, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn same_fill_in_new_batch_is_duplicate() {
    let f = fixture().await;
    let x = || fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z");
    ingest(&f.pool, &f.code, batch("b1", None, vec![x()])).await.unwrap();
    let r = ingest(&f.pool, &f.code, batch("b2", None, vec![x()])).await.unwrap();
    assert_eq!(outcomes(&r), ["duplicate"]);
    close(position(&f).await, (1.0, 100.0, 0.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn concurrent_same_batch_books_once() {
    let f = fixture().await;
    let b = || batch("b1", None, vec![fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z")]);
    let (a, c) = tokio::join!(ingest(&f.pool, &f.code, b()), ingest(&f.pool, &f.code, b()));
    assert_eq!(outcomes(&a.unwrap()), ["booked"]);
    assert_eq!(outcomes(&c.unwrap()), ["booked"]);
    close(position(&f).await, (1.0, 100.0, 0.0));
    assert_eq!(count(&f.pool, "SELECT count(*) FROM reported_fill").await, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn late_fill_placed_at_trade_time() {
    let f = fixture().await;
    ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BINANCE", "e2", "BUY", 1.0, 100.0, "2026-11-23T10:02:00Z"),
        fill("BINANCE", "e3", "SELL", 1.0, 110.0, "2026-11-23T10:03:00Z"),
    ])).await.unwrap();
    let r = ingest(&f.pool, &f.code, batch("b2", None, vec![
        fill("BINANCE", "e1", "BUY", 1.0, 90.0, "2026-11-23T10:01:00Z"),
    ])).await.unwrap();
    assert_eq!(outcomes(&r), ["booked"]);
    // in trade-time order: buy 90, buy 100 (avg 95), sell 1 @110 → realized 15
    close(position(&f).await, (1.0, 95.0, 15.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn cross_venue_aggregates_and_keeps_venue() {
    let f = fixture().await;
    ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"),
        fill("BYBIT", "e2", "BUY", 1.0, 200.0, "2026-11-23T10:01:00Z"),
    ])).await.unwrap();
    close(position(&f).await, (2.0, 150.0, 0.0));
    let venues: Vec<String> = sqlx::query_scalar("SELECT venue FROM reported_fill ORDER BY venue")
        .fetch_all(&f.pool).await.unwrap();
    assert_eq!(venues, ["BINANCE", "BYBIT"]);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM position").await, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn same_exec_id_on_two_venues_is_deterministic() {
    let f = fixture().await;
    // same trade time and exec id on two venues: order must not depend on insert order
    ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BYBIT", "x", "SELL", 1.0, 120.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    ingest(&f.pool, &f.code, batch("b2", None, vec![
        fill("BINANCE", "x", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    let incremental = position(&f).await;
    crate::positions::rebuild_positions_scoped(&f.pool, Some(f.portfolio)).await.unwrap();
    let rebuilt = position(&f).await;
    close(rebuilt, incremental);
    // BINANCE|main|x sorts first: buy 100 then sell 120 → flat, realized 20
    close(rebuilt, (0.0, 0.0, 20.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn unknown_instrument_becomes_break_rest_book() {
    let f = fixture().await;
    let mut bad = fill("BINANCE", "e2", "BUY", 1.0, 100.0, "2026-11-23T10:01:00Z");
    bad["symbol"] = json!("NOPEUSDT");
    let r = ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"),
        bad,
        fill("BINANCE", "e3", "BUY", 1.0, 100.0, "2026-11-23T10:02:00Z"),
    ])).await.unwrap();
    assert_eq!(outcomes(&r), ["booked", "break", "booked"]);
    let id = r.results[1].break_id.expect("break id");
    let (kind, source, strategy): (String, String, String) =
        sqlx::query_as("SELECT kind, source, strategy_code FROM recon_break WHERE id = $1")
            .bind(id).fetch_one(&f.pool).await.unwrap();
    assert_eq!((kind.as_str(), source.as_str(), strategy.as_str()), ("UNKNOWN_INSTRUMENT", "REPORTED", "rep"));
    close(position(&f).await, (2.0, 100.0, 0.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn malformed_fill_becomes_invalid_break() {
    let f = fixture().await;
    let r = ingest(&f.pool, &f.code, batch("b1", None, vec![
        json!({"venue": "BINANCE", "account_ref": "main", "exec_id": "e1", "symbol": "BTCUSDT", "side": "BUY"}),
        fill("BINANCE", "e2", "BUY", -1.0, 100.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    assert_eq!(outcomes(&r), ["break", "break"]);
    assert_eq!(r.results[0].exec_id.as_deref(), Some("e1"));
    assert_eq!(count(&f.pool, "SELECT count(*) FROM recon_break WHERE kind = 'INVALID_FILL'").await, 2);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM reported_fill").await, 0);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn conflicting_duplicate_becomes_break() {
    let f = fixture().await;
    ingest(&f.pool, &f.code, batch("b1", None, vec![
        fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    let r = ingest(&f.pool, &f.code, batch("b2", None, vec![
        fill("BINANCE", "e1", "BUY", 2.0, 100.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    assert_eq!(outcomes(&r), ["break"]);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM recon_break WHERE kind = 'CONFLICTING_DUPLICATE'").await, 1);
    close(position(&f).await, (1.0, 100.0, 0.0));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn retry_does_not_duplicate_break() {
    let f = fixture().await;
    let mut bad = fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z");
    bad["symbol"] = json!("NOPEUSDT");
    let a = ingest(&f.pool, &f.code, batch("b1", None, vec![bad.clone()])).await.unwrap();
    let b = ingest(&f.pool, &f.code, batch("b2", None, vec![bad])).await.unwrap();
    assert_eq!(a.results[0].break_id, b.results[0].break_id);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM recon_break").await, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn bnb_fee_books_with_other_effect() {
    let f = fixture().await;
    let mut x = fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z");
    x["fee"] = json!(0.001);
    x["fee_asset"] = json!("BNB");
    let r = ingest(&f.pool, &f.code, batch("b1", None, vec![x])).await.unwrap();
    assert_eq!(outcomes(&r), ["booked"]);
    close(position(&f).await, (1.0, 100.0, 0.0));
    let effect: Option<String> = sqlx::query_scalar("SELECT fee_effect FROM reported_fill")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(effect.as_deref(), Some("Other"));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn quote_and_base_fees_hit_position() {
    let f = fixture().await;
    let mut a = fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z");
    a["fee"] = json!(0.1);
    a["fee_asset"] = json!("BTC");
    let mut b = fill("BINANCE", "e2", "SELL", 0.5, 110.0, "2026-11-23T10:01:00Z");
    b["fee"] = json!(1.0);
    b["fee_asset"] = json!("USDT");
    ingest(&f.pool, &f.code, batch("b1", None, vec![a, b])).await.unwrap();
    let effects: Vec<String> = sqlx::query_scalar("SELECT fee_effect FROM reported_fill ORDER BY exec_id")
        .fetch_all(&f.pool).await.unwrap();
    assert_eq!(effects, ["Base", "Quote"]);
    let incremental = position(&f).await;
    crate::positions::rebuild_positions_scoped(&f.pool, Some(f.portfolio)).await.unwrap();
    close(position(&f).await, incremental);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn sequence_gap_flagged_not_rejected() {
    let f = fixture().await;
    let a = ingest(&f.pool, &f.code, batch("b1", Some(1), vec![
        fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"),
    ])).await.unwrap();
    let b = ingest(&f.pool, &f.code, batch("b2", Some(3), vec![
        fill("BINANCE", "e2", "BUY", 1.0, 100.0, "2026-11-23T10:01:00Z"),
    ])).await.unwrap();
    assert!(!a.sequence_gap);
    assert!(b.sequence_gap);
    assert_eq!(outcomes(&b), ["booked"]);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM reported_batch WHERE sequence_gap").await, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn payload_book_field_ignored() {
    let f = fixture().await;
    let other = new_strategy(&f.pool, "other").await;
    let mut x = fill("BINANCE", "e1", "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z");
    x["portfolio_id"] = json!(other.to_string());
    x["book"] = json!("RPT-other");
    let r = ingest(&f.pool, &f.code, batch("b1", None, vec![x])).await.unwrap();
    assert_eq!(outcomes(&r), ["booked"]);
    close(position(&f).await, (1.0, 100.0, 0.0));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM position WHERE portfolio_id = $1")
        .bind(other).fetch_one(&f.pool).await.unwrap();
    assert_eq!(n, 0);
}

// ---- HTTP gate ----

async fn token(pool: &PgPool, code: &str) -> String {
    let mut conn = pool.acquire().await.unwrap();
    super::token::issue(&mut conn, code).await.unwrap()
}

async fn post(pool: &PgPool, code: &str, bearer: &str, body: Value) -> StatusCode {
    let req = Request::post(format!("/api/v1/strategies/{code}/fills"))
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    api::router(pool.clone()).oneshot(req).await.unwrap().status()
}

fn body(fills: usize, version: u32) -> Value {
    let fills: Vec<Value> = (0..fills)
        .map(|i| fill("BINANCE", &format!("e{i}"), "BUY", 1.0, 100.0, "2026-11-23T10:00:00Z"))
        .collect();
    json!({ "schema_version": version, "batch_id": Uuid::new_v4().to_string(), "fills": fills })
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn revoked_token_is_401() {
    let f = fixture().await;
    let t = token(&f.pool, &f.code).await;
    assert_eq!(post(&f.pool, &f.code, &t, body(1, 1)).await, StatusCode::OK);
    sqlx::query("UPDATE strategy_reporting_token SET revoked_at = now()").execute(&f.pool).await.unwrap();
    assert_eq!(post(&f.pool, &f.code, &t, body(1, 1)).await, StatusCode::UNAUTHORIZED);
    assert_eq!(post(&f.pool, &f.code, "garbage", body(1, 1)).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn token_of_other_strategy_is_forbidden() {
    let f = fixture().await;
    new_strategy(&f.pool, "other").await;
    let t = token(&f.pool, "other").await;
    assert_eq!(post(&f.pool, &f.code, &t, body(1, 1)).await, StatusCode::FORBIDDEN);
    assert_eq!(count(&f.pool, "SELECT count(*) FROM reported_batch").await, 0);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn wrong_schema_version_is_400() {
    let f = fixture().await;
    let t = token(&f.pool, &f.code).await;
    assert_eq!(post(&f.pool, &f.code, &t, body(1, 2)).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn oversized_batch_is_413() {
    let f = fixture().await;
    let t = token(&f.pool, &f.code).await;
    assert_eq!(post(&f.pool, &f.code, &t, body(501, 1)).await, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn config_returns_limits() {
    let f = fixture().await;
    let t = token(&f.pool, &f.code).await;
    let req = Request::get(format!("/api/v1/strategies/{}/config", f.code))
        .header("authorization", format!("Bearer {t}"))
        .body(Body::empty())
        .unwrap();
    let res = api::router(f.pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v, json!({
        "strategy": "rep", "portfolio_code": "RPT-rep", "schema_version": 1,
        "max_batch_fills": 500, "max_batches_per_sec": 20,
    }));
}

#[test]
fn rate_limit_bucket_refills() {
    let code = format!("rl-{}", Uuid::new_v4());
    let allowed = (0..60).filter(|_| api::allow(&code)).count();
    assert_eq!(allowed, 40, "burst of 40");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn token_active_tracks_issue_and_revoke() {
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.unwrap();
    super::token::issue(&mut conn, &f.code).await.unwrap();
    drop(conn);
    assert!(super::status::token_active(&f.pool, &f.code).await.unwrap());
    sqlx::query("UPDATE strategy_reporting_token SET revoked_at = now() WHERE strategy_code = $1")
        .bind(&f.code).execute(&f.pool).await.unwrap();
    assert!(!super::status::token_active(&f.pool, &f.code).await.unwrap());
}
