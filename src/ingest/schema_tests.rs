//! DB tests for the external-orders/execution-recon schema (migrations 0066, 0067).

use sqlx::PgPool;
use uuid::Uuid;

use crate::authz_schema_tests::migrated_db;

/// A disposable database whose every connection resolves unqualified names in
/// `oms`, as the runtime role does. Triggers from older migrations (e.g. the FIX
/// report queue) name their tables unqualified, so a pool connection without
/// this search_path fails at random depending on which connection a query gets.
async fn fresh_db() -> PgPool {
    let pool = crate::authz_schema_tests::fresh_db().await;
    let name: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&pool).await.unwrap();
    pool.close().await;
    let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL").unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    PgPool::connect_with(options.database(&name).options([("search_path", "oms,public")])).await.unwrap()
}
use crate::setup::database::migrate;

const RECON_MIGRATION: &str = "0066_";

struct Base {
    connection: String,
    account: Uuid,
    portfolio: Uuid,
    principal: Uuid,
}

/// A connection, account, portfolio and principal, using only pre-0066 columns.
async fn seed_base(pool: &PgPool) -> Base {
    seed_base_for(pool, "BINANCE").await
}

async fn seed_base_for(pool: &PgPool, broker: &str) -> Base {
    let suffix = Uuid::new_v4().simple().to_string();
    let base = Base {
        connection: format!("conn-{suffix}"),
        account: Uuid::new_v4(),
        portfolio: Uuid::new_v4(),
        principal: Uuid::new_v4(),
    };
    sqlx::query("INSERT INTO oms.broker_connection (code, broker_code, environment) VALUES ($1, $2, 'PAPER')")
        .bind(&base.connection).bind(broker).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
        .bind(base.portfolio).bind(format!("pf-{suffix}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')")
        .bind(base.principal).bind(format!("pr-{suffix}")).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO oms.account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $3, $4, 'ACTIVE', $5)",
    )
    .bind(base.account).bind(format!("acc-{suffix}")).bind(&base.connection)
    .bind(format!("ext-{suffix}")).bind(base.portfolio)
    .execute(pool).await.unwrap();
    base
}

async fn insert_order(
    pool: &PgPool,
    b: &Base,
    id: Uuid,
    external_order_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oms.order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, original_qty, leaves_qty, \
         cum_qty, status, external_order_id, version) \
         VALUES ($1, $2, $3, $4, $5, $6, 'BTCUSDT', 'BUY', 'MARKET', 'GTC', 1, 0, 1, 'FILLED', $7, 1)",
    )
    .bind(id).bind(format!("c-{id}")).bind(b.principal).bind(b.portfolio).bind(b.account)
    .bind(&b.connection).bind(external_order_id)
    .execute(pool).await.map(|_| ())
}

async fn seed_order(pool: &PgPool, b: &Base, external_order_id: &str) -> Uuid {
    let id = Uuid::new_v4();
    insert_order(pool, b, id, external_order_id).await.unwrap();
    id
}

async fn insert_fill(pool: &PgPool, b: &Base, exec_id: &str, order: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO oms.venue_fill (broker_connection_code, account_id, exec_id, order_id) VALUES ($1, $2, $3, $4)")
        .bind(&b.connection).bind(b.account).bind(exec_id).bind(order)
        .execute(pool).await.map(|_| ())
}

async fn insert_break(pool: &PgPool, b: &Base, venue_order_id: &str, status: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO oms.recon_break (id, broker_connection_code, account_id, venue_order_id, kind, events_json, status) \
         VALUES ($1, $2, $3, $4, 'unknown_order', '[]'::jsonb, $5)",
    )
    .bind(Uuid::new_v4()).bind(&b.connection).bind(b.account).bind(venue_order_id).bind(status)
    .execute(pool).await.map(|_| ())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn migration_seeds_venue_fill_from_existing_fills() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < RECON_MIGRATION)
        .await
        .unwrap();

    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "venue-1").await;
    for (version, event_type, exec) in [(1, "order_partially_filled", "exec-1"), (2, "order_filled", "exec-2")] {
        sqlx::query(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, $2, $3, $4, jsonb_build_object('event_type', $4::text, \
                 'payload', jsonb_build_object('execution_id', $5::text)))",
        )
        .bind(order).bind(version).bind(Uuid::new_v4()).bind(event_type).bind(exec)
        .execute(&pool).await.unwrap();
    }
    // A non-fill event must not seed anything.
    sqlx::query(
        "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
         VALUES ($1, 3, $2, 'order_routed', '{\"payload\":{\"external_order_id\":\"venue-1\"}}')",
    )
    .bind(order).bind(Uuid::new_v4()).execute(&pool).await.unwrap();

    migrate::apply_all(&pool).await.unwrap();

    let seeded: Vec<(String, Uuid, Uuid)> = sqlx::query_as(
        "SELECT exec_id, order_id, account_id FROM oms.venue_fill WHERE broker_connection_code = $1 ORDER BY exec_id",
    )
    .bind(&b.connection).fetch_all(&pool).await.unwrap();
    assert_eq!(
        seeded,
        vec![("exec-1".to_string(), order, b.account), ("exec-2".to_string(), order, b.account)]
    );
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_colo_per_trade_rows_survive_and_are_seeded() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < RECON_MIGRATION)
        .await
        .unwrap();

    let b = seed_base(&pool).await;
    let trade_a = seed_order(&pool, &b, "shared").await;
    let trade_b = seed_order(&pool, &b, "shared").await;
    sqlx::query("UPDATE oms.order_state SET origin = 'EXTERNAL', venue_trade_id = 100 WHERE order_id = ANY($1)")
        .bind(vec![trade_a, trade_b])
        .execute(&pool).await.unwrap();
    for (order, exec) in [(trade_a, "t-1"), (trade_b, "t-2")] {
        sqlx::query(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, 1, $2, 'order_filled', jsonb_build_object('payload', jsonb_build_object('execution_id', $3::text)))",
        )
        .bind(order).bind(Uuid::new_v4()).bind(exec).execute(&pool).await.unwrap();
    }

    migrate::apply_all(&pool).await.unwrap();

    let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.order_state WHERE broker_connection_code = $1")
        .bind(&b.connection).fetch_one(&pool).await.unwrap();
    assert_eq!(orders, 2);
    let fills: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT exec_id, order_id FROM oms.venue_fill WHERE broker_connection_code = $1 ORDER BY exec_id",
    )
    .bind(&b.connection).fetch_all(&pool).await.unwrap();
    assert_eq!(fills, vec![("t-1".to_string(), trade_a), ("t-2".to_string(), trade_b)]);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn duplicate_venue_fill_is_rejected() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "venue-1").await;
    insert_fill(&pool, &b, "exec-1", order).await.unwrap();
    let err = insert_fill(&pool, &b, "exec-1", order).await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert_fill(&pool, &b, "exec-2", order).await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn one_open_break_per_venue_order() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    insert_break(&pool, &b, "v-1", "open").await.unwrap();
    let err = insert_break(&pool, &b, "v-1", "open").await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
    insert_break(&pool, &b, "v-2", "open").await.unwrap();
    sqlx::query("UPDATE oms.recon_break SET status = 'resolved', resolved_at = now() WHERE venue_order_id = 'v-1'")
        .execute(&pool).await.unwrap();
    insert_break(&pool, &b, "v-1", "open").await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn a_venue_order_id_maps_to_one_order_per_account() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    seed_order(&pool, &b, "venue-1").await;
    let err = insert_order(&pool, &b, Uuid::new_v4(), "venue-1").await.unwrap_err();
    assert!(is_unique_violation(&err), "{err}");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn connection_recon_settings_default_and_validate() {
    let pool = migrated_db().await;
    let b = seed_base(&pool).await;
    let (policy, lookback, poll): (String, String, Option<i32>) = sqlx::query_as(
        "SELECT external_policy, eod_lookback, intraday_poll_secs FROM oms.broker_connection WHERE code = $1",
    )
    .bind(&b.connection).fetch_one(&pool).await.unwrap();
    assert_eq!((policy.as_str(), lookback.as_str(), poll), ("adopt", "1w", None));
    let bad = sqlx::query("UPDATE oms.broker_connection SET external_policy = 'ignore' WHERE code = $1")
        .bind(&b.connection).execute(&pool).await;
    assert!(bad.is_err());
    let rule = sqlx::query(
        "INSERT INTO oms.attribution_rule (id, broker_connection_code, portfolio_id, priority) VALUES ($1, $2, $3, 1)",
    )
    .bind(Uuid::new_v4()).bind(&b.connection).bind(b.portfolio)
    .execute(&pool).await;
    assert!(rule.is_err(), "a rule needs an account or a client-id prefix");
}

/// One legacy fill: an order on `instrument` (`side`) with venue id `ext`, and a
/// fill event carrying `execution_id` and `cum_qty` (as JSON numbers are stored).
async fn legacy_fill(pool: &PgPool, b: &Base, instrument: &str, side: &str, ext: &str, fills: &[(&str, &str)]) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO oms.order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, original_qty, leaves_qty, \
         cum_qty, status, external_order_id, version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'MARKET', 'GTC', 1, 0, 1, 'FILLED', $9, 1)",
    )
    .bind(id).bind(format!("c-{id}")).bind(b.principal).bind(b.portfolio).bind(b.account)
    .bind(&b.connection).bind(instrument).bind(side).bind(ext)
    .execute(pool).await.unwrap();
    for (version, (exec, cum)) in fills.iter().enumerate() {
        sqlx::query(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
             VALUES ($1, $2, $3, 'order_partially_filled', jsonb_build_object('payload', \
                 jsonb_build_object('execution_id', $4::text, 'cum_qty', $5::text::jsonb)))",
        )
        .bind(id).bind(version as i64 + 1).bind(Uuid::new_v4()).bind(exec).bind(cum)
        .execute(pool).await.unwrap();
    }
    id
}

async fn seeded_ids(pool: &PgPool, b: &Base) -> Vec<String> {
    sqlx::query_scalar("SELECT exec_id FROM oms.venue_fill WHERE broker_connection_code = $1 ORDER BY exec_id")
        .bind(&b.connection).fetch_all(pool).await.unwrap()
}

async fn not_before(pool: &PgPool, b: &Base) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT recon_not_before FROM oms.broker_connection WHERE code = $1")
        .bind(&b.connection).fetch_one(pool).await.unwrap()
}

/// The ledger is seeded with the ids today's ingest paths report, so the first
/// sweep after the upgrade sees legacy fills as duplicates; where an id cannot
/// be re-keyed, the connection gets a `recon_not_before` watermark.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_seed_matches_new_exec_ids() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < RECON_MIGRATION)
        .await
        .unwrap();
    sqlx::query("INSERT INTO public.currency (code, name) VALUES ('USDT', 'Tether') ON CONFLICT DO NOTHING")
        .execute(&pool).await.unwrap();
    let venue: String = sqlx::query_scalar("SELECT code FROM public.venue ORDER BY code LIMIT 1").fetch_one(&pool).await.unwrap();
    let instrument = |symbol: &'static str| {
        let pool = pool.clone();
        let venue = venue.clone();
        async move {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO public.instrument (symbol, venue, name, asset_class, instrument_class, currency, status, price_precision, price_increment) \
                 VALUES ($1, $2, $1, 'CRYPTO', 'SPOT', 'USDT', 'ACTIVE', 2, 0.01) RETURNING id",
            )
            .bind(symbol).bind(&venue).fetch_one(&pool).await.unwrap();
            id
        }
    };
    let btc = instrument("BTCUSDT").await;
    let eth = instrument("ETH-USDT").await;
    for (broker, id, sym) in [("BINANCE", btc, "BTCUSDT"), ("OKX", eth, "ETH-USDT")] {
        sqlx::query("INSERT INTO public.broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, $3, true)")
            .bind(id).bind(broker).bind(sym).execute(&pool).await.unwrap();
    }

    let binance = seed_base_for(&pool, "BINANCE").await;
    legacy_fill(&pool, &binance, &btc.to_string(), "BUY", "9001", &[("501", "0.5")]).await;
    let okx = seed_base_for(&pool, "OKX").await;
    legacy_fill(&pool, &okx, &eth.to_string(), "SELL", "o1", &[("42", "1")]).await;
    let tws = seed_base_for(&pool, "IBKR_TWS").await;
    legacy_fill(&pool, &tws, "1", "BUY", "7", &[("0000e0d5.6578c1a2.01.01", "100")]).await;
    let alpaca = seed_base_for(&pool, "ALPACA").await;
    legacy_fill(&pool, &alpaca, "1", "BUY", "alp-1", &[("6f1d-uuid-a", "0.5"), ("6f1d-uuid-b", "10.0")]).await;
    let coinbase = seed_base_for(&pool, "COINBASE").await;
    legacy_fill(&pool, &coinbase, "1", "BUY", "cb-1", &[("b7c4e2a0-trade", "1")]).await;

    migrate::apply_all(&pool).await.unwrap();

    assert_eq!(seeded_ids(&pool, &binance).await, [gateway::connectors::binance::fill_exec_id("BTCUSDT", "501", true)]);
    assert_eq!(seeded_ids(&pool, &okx).await, ["ETH-USDT:42:S"]);
    assert_eq!(seeded_ids(&pool, &tws).await, ["0000e0d5.6578c1a2.01"]);
    // Alpaca's key is "{order id}:{cum}" with cum printed as Rust prints an f64.
    assert_eq!(seeded_ids(&pool, &alpaca).await, [format!("alp-1:{}", 0.5f64), format!("alp-1:{}", 10.0f64)]);
    assert_eq!(seeded_ids(&pool, &coinbase).await, ["b7c4e2a0-trade"]);

    // Binance FIX keyed fills on ExecID(17), which cannot be told from a trade
    // id: its connection is watermarked; the re-keyed venues are not.
    assert!(not_before(&pool, &binance).await.is_some());
    for b in [&okx, &tws, &alpaca, &coinbase] {
        assert_eq!(not_before(&pool, b).await, None, "{}", b.connection);
    }
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn trade_time_columns_exist_and_default_null() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "tt-1").await;
    sqlx::query(
        "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json) \
         VALUES ($1, 1, $2, 'order_filled', '{}'::jsonb)",
    )
    .bind(order).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO oms.position (portfolio_id, instrument_id) VALUES ($1, 'BTCUSDT')")
        .bind(b.portfolio).execute(&pool).await.unwrap();

    let occurred: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT occurred_at FROM oms.order_event WHERE order_id = $1")
            .bind(order).fetch_one(&pool).await.unwrap();
    assert_eq!(occurred, None);

    let (last, created, cum): (Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
        sqlx::query_as("SELECT last_trade_at, venue_created_at, venue_cum_qty::text FROM oms.order_state WHERE order_id = $1")
            .bind(order).fetch_one(&pool).await.unwrap();
    assert_eq!((last, created, cum), (None, None, None));

    let pos: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_trade_at FROM oms.position WHERE portfolio_id = $1")
            .bind(b.portfolio).fetch_one(&pool).await.unwrap();
    assert_eq!(pos, None);

    let idx: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE schemaname = 'oms' AND tablename = 'order_state' \
         AND indexdef LIKE '%last_trade_at DESC NULLS LAST%'",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(idx, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn order_event_created_at_is_timestamptz_and_legacy_rows_keep_utc_instant() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < "0067_").await.unwrap();

    // Before 0067 created_at is a zoneless UTC wall clock.
    let b = seed_base(&pool).await;
    let order = seed_order(&pool, &b, "tz-1").await;
    sqlx::query(
        "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json, created_at) \
         VALUES ($1, 1, $2, 'order_filled', '{}'::jsonb, TIMESTAMP '2026-01-01 12:00:00')",
    )
    .bind(order).bind(Uuid::new_v4()).execute(&pool).await.unwrap();

    migrate::apply_all(&pool).await.unwrap();

    let ty: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = 'oms' AND table_name = 'order_event' AND column_name = 'created_at'",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(ty, "timestamp with time zone");

    // The instant is the stored wall clock read as UTC, whatever the session zone.
    let expected = chrono::DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
    for zone in ["UTC", "Europe/Rome", "America/New_York"] {
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query(&format!("SET TimeZone = '{zone}'")).execute(&mut *conn).await.unwrap();
        let got: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT created_at FROM oms.order_event WHERE order_id = $1")
                .bind(order).fetch_one(&mut *conn).await.unwrap();
        assert_eq!(got, expected, "session zone {zone}");
    }
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_filled_order_gets_booking_time_as_trade_time() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < "0067_").await.unwrap();

    let b = seed_base(&pool).await;
    let filled = seed_order(&pool, &b, "lg-filled").await;
    let unfilled = seed_order(&pool, &b, "lg-open").await;
    for (v, ty, ts) in [(1, "order_partially_filled", "2026-01-01 12:00:00"), (2, "order_filled", "2026-01-02 08:30:00")] {
        sqlx::query(&format!(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json, created_at) \
             VALUES ($1, $2, $3, $4, '{{}}'::jsonb, TIMESTAMP '{ts}')"
        ))
        .bind(filled).bind(v).bind(Uuid::new_v4()).bind(ty).execute(&pool).await.unwrap();
    }

    migrate::apply_all(&pool).await.unwrap();

    let get = |o: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
                "SELECT last_trade_at FROM oms.order_state WHERE order_id = $1",
            )
            .bind(o).fetch_one(&pool).await.unwrap()
        }
    };
    let expected = chrono::DateTime::parse_from_rfc3339("2026-01-02T08:30:00Z").unwrap().with_timezone(&chrono::Utc);
    assert_eq!(get(filled).await, Some(expected));
    assert_eq!(get(unfilled).await, None);
}

/// A fill event's payload as the domain writes it (replay reads it back).
fn fill_payload(order: Uuid, version: i64, exec: &str, qty: f64, price: f64, at: Option<chrono::DateTime<chrono::Utc>>) -> serde_json::Value {
    use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload, OrderEventType};
    serde_json::to_value(OrderDomainEvent {
        event_id: Uuid::new_v4().to_string(),
        event_type: OrderEventType::OrderPartiallyFilled,
        order_id: order.to_string(),
        timestamp: chrono::Utc::now(),
        actor: "test".into(),
        payload: OrderEventPayload::OrderPartiallyFilled {
            execution_id: exec.into(), fill_qty: qty, fill_price: price, cum_qty: qty, leaves_qty: 0.0, avg_px: price, venue: "TEST".into(),
        },
        version,
        status_after: crate::domain::orders::state::OrderStatus::PartiallyFilled,
        occurred_at: at,
    })
    .unwrap()
}

/// A position booked before 0067 has no trade-time key. After the upgrade and the
/// boot rebuild, a fill older than its legacy inputs must rebuild the position, so
/// the result equals a full canonical rebuild.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_position_then_older_fill_matches_canonical_rebuild() {
    use crate::domain::orders::state::OrderSide;
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| schema != "oms" || file < "0067_").await.unwrap();

    let b = seed_base(&pool).await;
    let buy = seed_order(&pool, &b, "lp-buy").await;
    let sell = seed_order(&pool, &b, "lp-sell").await;
    sqlx::query("UPDATE oms.order_state SET side = 'buy' WHERE order_id = $1").bind(buy).execute(&pool).await.unwrap();
    sqlx::query("UPDATE oms.order_state SET side = 'sell' WHERE order_id = $1").bind(sell).execute(&pool).await.unwrap();
    // Legacy: buy 2 @ 100 booked 12:00, sell 1 @ 120 booked 13:00 → long 1 @ 100, realized 20.
    for (order, v, exec, qty, px, ts) in [(buy, 1, "lx-1", 2.0, 100.0, "2026-01-01 12:00:00"), (sell, 1, "lx-2", 1.0, 120.0, "2026-01-01 13:00:00")] {
        sqlx::query(&format!(
            "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json, created_at) \
             VALUES ($1, $2, $3, 'order_partially_filled', $4, TIMESTAMP '{ts}')"
        ))
        .bind(order).bind(v).bind(Uuid::new_v4()).bind(fill_payload(order, v, exec, qty, px, None))
        .execute(&pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO oms.position (portfolio_id, instrument_id, net_qty, avg_cost, realized_pnl) \
         VALUES ($1, 'BTCUSDT', 1, 100, 20)",
    )
    .bind(b.portfolio).execute(&pool).await.unwrap();

    migrate::apply_all(&pool).await.unwrap();
    crate::positions::rebuild_if_empty(&pool).await.unwrap();

    // A late buy 1 @ 50 traded 11:00, before both legacy fills.
    let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T11:00:00Z").unwrap().with_timezone(&chrono::Utc);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO oms.order_event (order_id, version, event_id, event_type, payload_json, occurred_at) \
         VALUES ($1, 2, $2, 'order_partially_filled', $3, $4)",
    )
    .bind(buy).bind(Uuid::new_v4()).bind(fill_payload(buy, 2, "lx-0", 1.0, 50.0, Some(at))).bind(at)
    .execute(&mut *tx).await.unwrap();
    crate::positions::persist_fill(&mut tx, b.portfolio, "BTCUSDT", OrderSide::Buy, 1.0, 50.0, at, "lx-0").await.unwrap();
    tx.commit().await.unwrap();

    let read = || {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (f64, f64, f64)>(
                "SELECT net_qty::float8, avg_cost::float8, realized_pnl::float8 FROM oms.position \
                 WHERE portfolio_id = $1 AND instrument_id = 'BTCUSDT'",
            )
            .bind(b.portfolio).fetch_one(&pool).await.unwrap()
        }
    };
    let incremental = read().await;
    crate::positions::rebuild_positions(&pool).await.unwrap();
    let canonical = read().await;
    // Canonical: buy 1@50, buy 2@100 → 3 @ 83.33; sell 1 @ 120 → realized 36.67.
    assert!((canonical.1 - 250.0 / 3.0).abs() < 1e-9, "canonical {canonical:?}");
    assert_eq!(incremental, canonical);
}
