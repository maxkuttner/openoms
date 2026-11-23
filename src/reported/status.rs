//! Session-auth views of a reported strategy: `/strategies/reported/:code/{status,fills,batches}`
//! and `PATCH /strategies/reported/:code`.
//!
//! Readable by the owner, an admin, or anyone with `can_view` on the strategy's
//! portfolio (`portfolio_access`, which covers group members). Changing the
//! staleness threshold is owner/admin only.
//!
//! Gap "expected" is derived, not stored: for a batch flagged `sequence_gap`,
//! it is the highest sequence among batches received before it, plus one
//! (the same rule ingest used to set the flag).

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::strategy::ReportedStrategy;
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::{internal, ApiError};

fn db(e: sqlx::Error) -> ApiError {
    internal("database error", e)
}

fn fail(status: StatusCode, message: &str) -> ApiError {
    ApiError { status, message: message.into() }
}

async fn is_admin(pool: &PgPool, id: Uuid) -> Result<bool, ApiError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principal_admin WHERE principal_id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(db)
}

/// (owner, portfolio) of a reported strategy; 404 if there is none.
async fn load(pool: &PgPool, code: &str) -> Result<(Uuid, Uuid), ApiError> {
    sqlx::query_as("SELECT owner_principal_id, portfolio_id FROM strategy WHERE code = $1 AND mode = 'REPORTED'")
        .bind(code)
        .fetch_optional(pool)
        .await
        .map_err(db)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "reported strategy not found"))
}

/// 404/403 unless the caller may view the strategy.
async fn require_view(pool: &PgPool, caller: Uuid, code: &str) -> Result<(), ApiError> {
    let (owner, portfolio) = load(pool, code).await?;
    if owner == caller || is_admin(pool, caller).await? {
        return Ok(());
    }
    let can: Option<bool> =
        sqlx::query_scalar("SELECT bool_or(can_view) FROM portfolio_access WHERE principal_id = $1 AND portfolio_id = $2")
            .bind(caller)
            .bind(portfolio)
            .fetch_one(pool)
            .await
            .map_err(db)?;
    if can == Some(true) {
        Ok(())
    } else {
        Err(fail(StatusCode::FORBIDDEN, "not authorized"))
    }
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Gap {
    pub batch_id: String,
    pub sequence: i64,
    pub expected: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Status {
    pub last_report_at: Option<DateTime<Utc>>,
    /// Never reported, or the last report is older than `stale_after_secs`.
    pub stale: bool,
    pub last_sequence: Option<i64>,
    pub gaps: Vec<Gap>,
    pub open_breaks: i64,
    /// True while the strategy holds an unrevoked reporting token.
    pub token_active: bool,
}

/// Whether `code` has a reporting token that has not been revoked.
pub async fn token_active(pool: &PgPool, code: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM strategy_reporting_token WHERE strategy_code = $1 AND revoked_at IS NULL)",
    )
    .bind(code)
    .fetch_one(pool)
    .await
}

pub async fn get_status(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
) -> Result<Json<Status>, ApiError> {
    let pool = state.pool();
    require_view(pool, auth.principal_id, &code).await?;
    let (last_report_at, stale, last_sequence): (Option<DateTime<Utc>>, bool, Option<i64>) = sqlx::query_as(
        "SELECT b.last, b.last IS NULL OR now() - b.last > make_interval(secs => s.stale_after_secs), b.seq \
           FROM strategy s, \
                LATERAL (SELECT max(received_at) AS last, max(sequence) AS seq \
                           FROM reported_batch WHERE strategy_code = s.code) b \
          WHERE s.code = $1",
    )
    .bind(&code)
    .fetch_one(pool)
    .await
    .map_err(db)?;
    let gaps = sqlx::query_as::<_, Gap>(
        "SELECT batch_id, sequence, expected FROM ( \
           SELECT batch_id, sequence, sequence_gap, received_at, \
                  max(sequence) OVER (ORDER BY received_at, batch_id \
                                      ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) + 1 AS expected \
             FROM reported_batch WHERE strategy_code = $1) t \
          WHERE sequence_gap AND sequence IS NOT NULL AND expected IS NOT NULL \
          ORDER BY received_at DESC, batch_id DESC LIMIT 100",
    )
    .bind(&code)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let mut gaps = gaps;
    gaps.reverse(); // oldest first within the latest 100
    let open_breaks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM recon_break WHERE source = 'REPORTED' AND strategy_code = $1 AND status = 'open'",
    )
    .bind(&code)
    .fetch_one(pool)
    .await
    .map_err(db)?;
    let token_active = token_active(pool, &code).await.map_err(db)?;
    Ok(Json(Status { last_report_at, stale, last_sequence, gaps, open_breaks, token_active }))
}

#[derive(Debug, Deserialize)]
pub struct FillsQuery {
    pub limit: Option<i64>,
    /// Opaque paging cursor: the previous page's `next_before`.
    pub before: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FillsPage {
    pub fills: Vec<FillRow>,
    /// Pass as `before` for the next page; null on the last page.
    pub next_before: Option<String>,
}

/// `<rfc3339 trade_time>|<fill id>`: the (trade_time, id) sort key of the last row.
fn parse_cursor(c: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let (t, id) = c.split_once('|')?;
    Some((DateTime::parse_from_rfc3339(t).ok()?.with_timezone(&Utc), id.parse().ok()?))
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct FillRow {
    pub id: Uuid,
    pub batch_id: String,
    pub venue: String,
    pub account_ref: String,
    pub exec_id: String,
    pub client_order_id: Option<String>,
    pub symbol: String,
    pub instrument_id: i64,
    pub side: String,
    pub qty: f64,
    pub price: f64,
    pub fee: Option<f64>,
    pub fee_asset: Option<String>,
    pub fee_effect: Option<String>,
    pub trade_time: DateTime<Utc>,
    pub booked_at: DateTime<Utc>,
}

fn clamp(limit: Option<i64>) -> i64 {
    limit.unwrap_or(100).clamp(1, 500)
}

pub async fn get_fills(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
    Query(q): Query<FillsQuery>,
) -> Result<Json<FillsPage>, ApiError> {
    let pool = state.pool();
    require_view(pool, auth.principal_id, &code).await?;
    let cursor = match q.before.as_deref() {
        Some(c) => Some(parse_cursor(c).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "invalid before cursor"))?),
        None => None,
    };
    let limit = clamp(q.limit);
    let rows = sqlx::query_as::<_, FillRow>(
        "SELECT id, batch_id, venue, account_ref, exec_id, client_order_id, symbol, instrument_id, side, qty::float8 AS qty, \
                price::float8 AS price, fee::float8 AS fee, fee_asset, fee_effect, trade_time, booked_at \
           FROM reported_fill WHERE strategy_code = $1 AND ($2::timestamptz IS NULL OR (trade_time, id) < ($2, $3)) \
          ORDER BY trade_time DESC, id DESC LIMIT $4",
    )
    .bind(&code)
    .bind(cursor.map(|c| c.0))
    .bind(cursor.map(|c| c.1))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let next_before = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|r| format!("{}|{}", r.trade_time.to_rfc3339_opts(chrono::SecondsFormat::Micros, true), r.id)))
        .flatten();
    Ok(Json(FillsPage { fills: rows, next_before }))
}

#[derive(Debug, Deserialize)]
pub struct BatchesQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct BatchRow {
    pub batch_id: String,
    pub sequence: Option<i64>,
    pub received_at: DateTime<Utc>,
    pub booked: i32,
    pub duplicate: i32,
    pub broken: i32,
    pub sequence_gap: bool,
}

pub async fn get_batches(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
    Query(q): Query<BatchesQuery>,
) -> Result<Json<Vec<BatchRow>>, ApiError> {
    let pool = state.pool();
    require_view(pool, auth.principal_id, &code).await?;
    let rows = sqlx::query_as::<_, BatchRow>(
        "SELECT batch_id, sequence, received_at, booked, duplicate, broken, sequence_gap \
           FROM reported_batch WHERE strategy_code = $1 ORDER BY received_at DESC, batch_id LIMIT $2",
    )
    .bind(&code)
    .bind(clamp(q.limit))
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PatchReported {
    pub stale_after_secs: Option<i32>,
}

/// Owner or admin only.
pub async fn patch(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
    Json(p): Json<PatchReported>,
) -> Result<Json<ReportedStrategy>, ApiError> {
    crate::authz::require_mfa(&auth)?;
    let pool = state.pool();
    let (owner, _) = load(pool, &code).await?;
    if owner != auth.principal_id && !is_admin(pool, auth.principal_id).await? {
        return Err(fail(StatusCode::FORBIDDEN, "not authorized"));
    }
    if p.stale_after_secs.is_some_and(|s| s <= 0) {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "stale_after_secs must be positive"));
    }
    let row = sqlx::query_as::<_, ReportedStrategy>(
        "UPDATE strategy SET stale_after_secs = COALESCE($2, stale_after_secs) WHERE code = $1 \
         RETURNING code, name, portfolio_id, owner_principal_id, state, stale_after_secs, created_at",
    )
    .bind(&code)
    .bind(p.stale_after_secs)
    .fetch_one(pool)
    .await
    .map_err(db)?;
    Ok(Json(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use crate::reported::ingest::{ingest, FillBatch};
    use crate::reported::strategy::{create, CreateReported};
    use crate::strategies::test_support as ts;

    fn ctx(id: Uuid) -> Extension<AuthContext> {
        Extension(AuthContext::machine(id, "t".into()))
    }

    async fn human(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(id)
            .bind(format!("h-{id}"))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn setup(code: &str) -> (PgPool, AppState, Uuid, super::super::strategy::ReportedCreated) {
        let pool = migrated_db().await;
        let state = ts::state(pool.clone());
        let me = human(&pool).await;
        let Json(c) = create(
            State(state.clone()),
            ctx(me),
            Json(CreateReported { code: code.into(), name: "n".into(), base_currency: Some("USD".into()) }),
        )
        .await
        .unwrap();
        (pool, state, me, c)
    }

    async fn report(pool: &PgPool, code: &str, id: &str, seq: i64) {
        let b = FillBatch { schema_version: 1, batch_id: id.into(), sequence: Some(seq), fills: vec![] };
        ingest(pool, code, b).await.unwrap();
        // Distinct, ordered receipt times (one transaction clock tick can tie).
        sqlx::query("UPDATE reported_batch SET received_at = now() - make_interval(secs => 100 - $3) WHERE strategy_code = $1 AND batch_id = $2")
            .bind(code).bind(id).bind(seq as f64).execute(pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn status_reports_stale_after_threshold() {
        let (pool, state, me, _) = setup("st1").await;
        let Json(s) = get_status(State(state.clone()), ctx(me), Path("st1".into())).await.unwrap();
        assert!(s.stale && s.last_report_at.is_none() && s.last_sequence.is_none());
        report(&pool, "st1", "b1", 1).await;
        let Json(s) = get_status(State(state.clone()), ctx(me), Path("st1".into())).await.unwrap();
        assert!(!s.stale);
        assert_eq!(s.last_sequence, Some(1));
        sqlx::query("UPDATE reported_batch SET received_at = now() - interval '10 minutes'")
            .execute(&pool).await.unwrap();
        let Json(s) = get_status(State(state.clone()), ctx(me), Path("st1".into())).await.unwrap();
        assert!(s.stale);
        let Json(p) = patch(State(state.clone()), ctx(me), Path("st1".into()), Json(PatchReported { stale_after_secs: Some(3600) }))
            .await.unwrap();
        assert_eq!(p.stale_after_secs, 3600);
        let Json(s) = get_status(State(state.clone()), ctx(me), Path("st1".into())).await.unwrap();
        assert!(!s.stale);
        let err = patch(State(state), ctx(me), Path("st1".into()), Json(PatchReported { stale_after_secs: Some(0) }))
            .await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn status_lists_sequence_gaps() {
        let (pool, state, me, _) = setup("st2").await;
        report(&pool, "st2", "b1", 1).await;
        report(&pool, "st2", "b2", 2).await;
        report(&pool, "st2", "b5", 5).await;
        let Json(s) = get_status(State(state.clone()), ctx(me), Path("st2".into())).await.unwrap();
        assert_eq!(s.last_sequence, Some(5));
        assert_eq!(s.gaps.len(), 1);
        assert_eq!((s.gaps[0].batch_id.as_str(), s.gaps[0].sequence, s.gaps[0].expected), ("b5", 5, 3));
        assert_eq!(s.open_breaks, 0);
        let Json(b) = get_batches(State(state), ctx(me), Path("st2".into()), Query(BatchesQuery { limit: Some(2) }))
            .await.unwrap();
        assert_eq!(b.iter().map(|r| r.batch_id.as_str()).collect::<Vec<_>>(), vec!["b5", "b2"]);
        assert!(b[0].sequence_gap);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn fills_visible_to_group_member_not_stranger() {
        let (pool, state, me, c) = setup("st3").await;
        let member = human(&pool).await;
        let stranger = human(&pool).await;
        sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)")
            .bind(c.group_id).bind(member).execute(&pool).await.unwrap();
        let q = || Query(FillsQuery { limit: None, before: None });
        assert!(get_fills(State(state.clone()), ctx(me), Path("st3".into()), q()).await.is_ok());
        assert!(get_fills(State(state.clone()), ctx(member), Path("st3".into()), q()).await.is_ok());
        let err = get_fills(State(state.clone()), ctx(stranger), Path("st3".into()), q()).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let err = get_status(State(state.clone()), ctx(stranger), Path("st3".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let err = patch(State(state.clone()), ctx(member), Path("st3".into()), Json(PatchReported { stale_after_secs: Some(60) }))
            .await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let err = get_fills(State(state), ctx(me), Path("nope".into()), q()).await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn fills_paging_with_equal_trade_times_skips_nothing() {
        let (pool, state, me, _) = setup("st4").await;
        report(&pool, "st4", "b1", 1).await;
        sqlx::query("INSERT INTO venue (code, name) VALUES ('CRYPTO', 'c') ON CONFLICT DO NOTHING").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO currency (code, name) VALUES ('USDT', 'tether') ON CONFLICT DO NOTHING").execute(&pool).await.unwrap();
        let inst: i64 = sqlx::query_scalar(
            "INSERT INTO instrument (symbol, name, venue, asset_class, instrument_class, currency, price_precision, price_increment) \
             VALUES ('BTCUSDT', 'btc', 'CRYPTO', 'CRYPTO', 'SPOT', 'USDT', 2, 0.01) RETURNING id",
        ).fetch_one(&pool).await.unwrap();
        for i in 0..5 {
            sqlx::query(
                "INSERT INTO reported_fill (id, strategy_code, batch_id, venue, account_ref, exec_id, symbol, instrument_id, \
                   side, qty, price, trade_time, raw_json) \
                 VALUES ($1, 'st4', 'b1', 'BINANCE', 'a', $2, 'BTCUSDT', $3, 'BUY', 1, 1, '2026-01-01T00:00:00Z', '{}')",
            ).bind(Uuid::new_v4()).bind(format!("e{i}")).bind(inst).execute(&pool).await.unwrap();
        }
        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let Json(page) = get_fills(State(state.clone()), ctx(me), Path("st4".into()), Query(FillsQuery { limit: Some(2), before })).await.unwrap();
            seen.extend(page.fills.iter().map(|f| f.exec_id.clone()));
            before = page.next_before;
            if before.is_none() { break; }
        }
        seen.sort();
        assert_eq!(seen, vec!["e0", "e1", "e2", "e3", "e4"]);
    }
}
