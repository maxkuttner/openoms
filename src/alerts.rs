//! Generic alert channel: a row in `alert`, published to Kafka by the outbox bridge
//! (`published_at IS NULL` is the queue), announced to open cockpits as a
//! `UiNotice::Alerts`, and listed/acknowledged over REST. The first user is the
//! `ROLL_DUE` warning raised by the hourly expiry sweep.

use std::sync::OnceLock;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::authz::{self, Permission};
use crate::event_bus::EventBus;
use crate::futures::front::{roll_date, venue_today};
use crate::handlers::ApiError;

pub const ROLL_DUE: &str = "ROLL_DUE";

/// Where a raised alert is announced. Set once at boot; unset (tests, CLI), raising
/// still writes the row and the Kafka bridge still publishes it.
static BUS: OnceLock<EventBus> = OnceLock::new();

pub fn install_bus(bus: EventBus) {
    let _ = BUS.set(bus);
}

pub struct NewAlert {
    pub kind: &'static str,
    pub portfolio_id: Uuid,
    pub instrument_id: i64,
    pub dedupe_key: String,
    pub payload: Value,
}

/// Raise an alert. `None` when one with the same `dedupe_key` already exists
/// (acknowledged or not), so a sweep that re-runs never re-raises.
pub async fn raise(pool: &PgPool, a: NewAlert) -> Result<Option<Uuid>, sqlx::Error> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO alert (id, kind, portfolio_id, instrument_id, dedupe_key, payload) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (dedupe_key) DO NOTHING RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(a.kind)
    .bind(a.portfolio_id)
    .bind(a.instrument_id)
    .bind(&a.dedupe_key)
    .bind(&a.payload)
    .fetch_optional(pool)
    .await?;
    if id.is_some() {
        if let Some(bus) = BUS.get() {
            bus.publish_alert(a.portfolio_id);
        }
    }
    Ok(id)
}

/// Warn about held futures contracts that are within `roll_warn_days` of their roll date
/// (or past it) and still ACTIVE. Returns how many alerts were newly raised.
///
/// The dedupe key is per (portfolio, contract): one warning per position per contract,
/// however many sweeps run, and it survives restarts. Flat positions never warn.
pub async fn roll_warnings(pool: &PgPool, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let today = venue_today(now);
    #[allow(clippy::type_complexity)]
    let held: Vec<(Uuid, i64, String, String, NaiveDate, Option<DateTime<Utc>>, f64, i32, i32)> = sqlx::query_as(
        "SELECT p.portfolio_id, i.id, i.symbol, d.underlying_symbol, d.expiry_date, d.expires_at, \
                p.net_qty::float8, r.roll_offset_days, r.roll_warn_days \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         JOIN instrument_derivative d ON d.instrument_id = i.id \
         JOIN future_root r ON r.code = d.underlying_symbol \
         WHERE p.net_qty <> 0 AND i.instrument_class = 'FUTURE' AND i.status = 'ACTIVE' \
           AND d.expiry_date IS NOT NULL \
         ORDER BY p.portfolio_id, d.expiry_date, i.id",
    )
    .fetch_all(pool)
    .await?;

    let mut raised = 0;
    for (portfolio_id, instrument_id, contract, root, expiry, expires_at, net_qty, offset, warn) in held {
        let roll = roll_date(expiry, offset);
        if today < roll - chrono::Duration::days(i64::from(warn)) {
            continue;
        }
        // the root's contract after the held one, by expiry; none listed yet is null
        let next: Option<(i64, String)> = sqlx::query_as(
            "SELECT i.id, i.symbol FROM instrument i \
             JOIN instrument_derivative d ON d.instrument_id = i.id \
             WHERE i.instrument_class = 'FUTURE' AND i.status = 'ACTIVE' \
               AND d.underlying_symbol = $1 AND d.expiry_date > $2 \
             ORDER BY d.expiry_date, i.id LIMIT 1",
        )
        .bind(&root)
        .bind(expiry)
        .fetch_optional(pool)
        .await?;
        let alert = NewAlert {
            kind: ROLL_DUE,
            portfolio_id,
            instrument_id,
            dedupe_key: format!("{ROLL_DUE}:{portfolio_id}:{instrument_id}"),
            payload: json!({
                "root": root,
                "contract": contract,
                "next_contract": next.as_ref().map(|n| &n.1),
                "next_instrument_id": next.as_ref().map(|n| n.0),
                "net_qty": net_qty,
                "roll_date": roll,
                "expires_at": expires_at,
            }),
        };
        if raise(pool, alert).await?.is_some() {
            raised += 1;
        }
    }
    Ok(raised)
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AlertView {
    pub id: Uuid,
    pub kind: String,
    pub portfolio_id: Uuid,
    pub instrument_id: Option<i64>,
    /// Kind-specific detail; `ROLL_DUE` carries root, contract, next_contract,
    /// next_instrument_id, net_qty, roll_date and expires_at.
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

fn internal(what: &str, e: sqlx::Error) -> ApiError {
    ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("{what}: {e:?}") }
}

/// List unacknowledged alerts on the portfolios the caller can view
#[utoipa::path(
    get, path = "/alerts", tag = "orders",
    responses(
        (status = 200, description = "Open alerts, newest first", body = [AlertView]),
        (status = 401, description = "No credential"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_alerts(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<AlertView>>, ApiError> {
    let visible = authz::visible_portfolios(state.pool(), auth.principal_id, Permission::PositionsView)
        .await
        .map_err(|e| internal("failed to load portfolio access", e))?;
    let rows: Vec<(Uuid, String, Uuid, Option<i64>, Value, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, kind, portfolio_id, instrument_id, payload, created_at FROM alert \
         WHERE acknowledged_at IS NULL AND portfolio_id = ANY($1) ORDER BY created_at DESC, id",
    )
    .bind(&visible)
    .fetch_all(state.pool())
    .await
    .map_err(|e| internal("failed to list alerts", e))?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, kind, portfolio_id, instrument_id, payload, created_at)| AlertView {
                id,
                kind,
                portfolio_id,
                instrument_id,
                payload,
                created_at,
            })
            .collect(),
    ))
}

/// Acknowledge an alert so it stops being listed
#[utoipa::path(
    post, path = "/alerts/{id}/ack", tag = "orders",
    params(("id" = Uuid, Path, description = "Alert ID")),
    responses(
        (status = 204, description = "Acknowledged (also when it already was)"),
        (status = 404, description = "No such alert, or on a portfolio the caller cannot view"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn ack_alert(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let visible = authz::visible_portfolios(state.pool(), auth.principal_id, Permission::PositionsView)
        .await
        .map_err(|e| internal("failed to load portfolio access", e))?;
    let found: Option<Uuid> = sqlx::query_scalar(
        "UPDATE alert SET acknowledged_at = COALESCE(acknowledged_at, now()) \
         WHERE id = $1 AND portfolio_id = ANY($2) RETURNING id",
    )
    .bind(id)
    .bind(&visible)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| internal("failed to acknowledge alert", e))?;
    match found {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(ApiError { status: StatusCode::NOT_FOUND, message: "alert not found".into() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kafka_outbox::{drain, OutboxRecord, OutboxSink, OutboxTopics};
    use chrono::NaiveDate;

    static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn isolated_pool() -> PgPool {
        let url = std::env::var("OMS_TEST_DATABASE_URL").expect("set OMS_TEST_DATABASE_URL to a disposable database");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect isolated test database")
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Clean slate: ES futures ESZ6 (2026-12-18) and ESH7 (2027-03-19), one portfolio
    /// holding `qty` ESZ6. Returns (portfolio, ESZ6 id).
    async fn seed(pool: &PgPool, qty: f64) -> (Uuid, i64) {
        for sql in [
            "DELETE FROM alert",
            "DELETE FROM kafka_position_outbox",
            "DELETE FROM position",
            "DELETE FROM instrument WHERE id IN (SELECT instrument_id FROM instrument_derivative WHERE underlying_symbol = 'ES')",
        ] {
            sqlx::query(sql).execute(pool).await.unwrap();
        }
        let mut ids = vec![];
        for (symbol, expiry) in [("ESZ6", NaiveDate::from_ymd_opt(2026, 12, 18)), ("ESH7", NaiveDate::from_ymd_opt(2027, 3, 19))] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, contract_size, price_precision, price_increment) \
                 VALUES ($1, 'XCME', $1, 'INDEX', 'FUTURE', 'USD', 'ACTIVE', 50, 2, 0.25) RETURNING id",
            )
            .bind(symbol)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO instrument_derivative (instrument_id, underlying_symbol, expiry_date) VALUES ($1, 'ES', $2)")
                .bind(id)
                .bind(expiry)
                .execute(pool)
                .await
                .unwrap();
            ids.push(id);
        }
        let portfolio = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
            .bind(portfolio)
            .bind(format!("ALERT{}", &portfolio.simple().to_string()[..8]))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO position (portfolio_id, instrument_id, net_qty, avg_cost) VALUES ($1, $2, $3, 5000)")
            .bind(portfolio)
            .bind(ids[0].to_string())
            .bind(qty)
            .execute(pool)
            .await
            .unwrap();
        (portfolio, ids[0])
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn roll_due_fires_once_per_position() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        let (portfolio, esz6) = seed(&pool, 4.0).await;
        assert_eq!(roll_warnings(&pool, t("2026-12-07T15:00:00Z")).await.unwrap(), 1);
        assert_eq!(roll_warnings(&pool, t("2026-12-07T15:00:00Z")).await.unwrap(), 0);
        let (kind, key, payload): (String, String, Value) =
            sqlx::query_as("SELECT kind, dedupe_key, payload FROM alert WHERE portfolio_id = $1")
                .bind(portfolio)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, "ROLL_DUE");
        assert_eq!(key, format!("ROLL_DUE:{portfolio}:{esz6}"));
        assert_eq!(payload["root"], "ES");
        assert_eq!(payload["contract"], "ESZ6");
        assert_eq!(payload["next_contract"], "ESH7");
        assert_eq!(payload["net_qty"], 4.0);
        assert_eq!(payload["roll_date"], "2026-12-10");
        assert!(payload["next_instrument_id"].is_i64());
        assert!(payload["expires_at"].is_null() || payload["expires_at"].is_string());
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn roll_due_waits_for_the_window() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        seed(&pool, 4.0).await;
        assert_eq!(roll_warnings(&pool, t("2026-12-06T15:00:00Z")).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn flat_position_gets_no_warning() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        seed(&pool, 0.0).await;
        assert_eq!(roll_warnings(&pool, t("2026-12-09T15:00:00Z")).await.unwrap(), 0);
    }

    #[derive(Default)]
    struct Collect(std::sync::Mutex<Vec<OutboxRecord>>);

    #[async_trait::async_trait]
    impl OutboxSink for Collect {
        async fn send(&self, record: &OutboxRecord) -> Result<(), String> {
            self.0.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn alerts_are_published_then_marked() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        let (portfolio, _) = seed(&pool, 4.0).await;
        roll_warnings(&pool, t("2026-12-08T15:00:00Z")).await.unwrap();
        let topics = OutboxTopics { orders: "o".into(), fills: "f".into(), positions: "p".into(), alerts: "alerts".into() };
        let sink = Collect::default();
        drain(&pool, &sink, &topics, 100).await.unwrap();
        let sent: Vec<OutboxRecord> = sink.0.lock().unwrap().iter().filter(|r| r.topic == "alerts").cloned().collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].key, portfolio.to_string());
        let body: Value = serde_json::from_str(&sent[0].payload).unwrap();
        assert_eq!(body["kind"], "ROLL_DUE");
        assert_eq!(body["payload"]["contract"], "ESZ6");
        let unpublished: i64 = sqlx::query_scalar("SELECT count(*) FROM alert WHERE published_at IS NULL").fetch_one(&pool).await.unwrap();
        assert_eq!(unpublished, 0);
        // a second drain sends nothing more
        drain(&pool, &sink, &topics, 100).await.unwrap();
        assert_eq!(sink.0.lock().unwrap().iter().filter(|r| r.topic == "alerts").count(), 1);
    }
}
