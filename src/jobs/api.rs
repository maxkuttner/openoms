//! Admin API for jobs and broker connections' instrument universes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::{latest, CreateError, JobKind, JobLogLine, JobRecord, JobStatus};
use crate::app_state::AppState;
use crate::credentials::CredentialState;
use crate::secrets::MasterKey;
use crate::setup::brokers::Broker;

/// Where a broker connection's instrument universe stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UniverseState {
    NotLoaded,
    Loading,
    Loaded,
    Partial,
    Failed,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UniverseStatus {
    pub state: UniverseState,
    /// Instruments currently mapped to this connection's broker.
    pub count: i64,
    /// Why the state is `partial` or `failed`: "interrupted", "10 skipped: …", or
    /// the load's error.
    pub reason: Option<String>,
    /// The most recent universe load for this connection, if any.
    pub last_job: Option<JobRecord>,
}

/// The universe state from the latest load job (if any) and the number of
/// instruments currently mapped to the broker.
///
/// No job but instruments present means the universe was loaded by the CLI
/// before jobs existed: that is a loaded universe.
pub fn derive_universe_state(last_job: Option<&JobRecord>, count: i64) -> (UniverseState, Option<String>) {
    let Some(job) = last_job else {
        return if count > 0 { (UniverseState::Loaded, None) } else { (UniverseState::NotLoaded, None) };
    };
    match job.status {
        JobStatus::Queued | JobStatus::Running => (UniverseState::Loading, None),
        JobStatus::Failed => (UniverseState::Failed, job.error.clone()),
        JobStatus::Interrupted => (UniverseState::Partial, Some("interrupted".to_string())),
        JobStatus::Succeeded => {
            let result = job.result.as_ref();
            let skipped = result.and_then(|r| r["skipped"].as_i64()).unwrap_or(0);
            if skipped > 0 {
                let reasons: Vec<&str> = result
                    .and_then(|r| r["skipped_reasons"].as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                (UniverseState::Partial, Some(format!("{skipped} skipped: {}", reasons.join(", "))))
            } else if count > 0 {
                (UniverseState::Loaded, None)
            } else {
                (UniverseState::NotLoaded, None)
            }
        }
    }
}

/// Why a universe load was not started.
#[derive(Debug)]
pub enum LoadRefusal {
    NotFound,
    AlreadyRunning(Uuid),
    /// The connection cannot be loaded as it is: no usable credentials, a broker
    /// without a catalog, or not the active connection for its broker.
    Unprocessable(String),
    Internal(String),
}

impl IntoResponse for LoadRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "broker_connection not found").into_response(),
            Self::AlreadyRunning(id) => (
                StatusCode::CONFLICT,
                Json(json!({"error": "a universe load is already running", "job_id": id})),
            )
                .into_response(),
            Self::Unprocessable(m) => (StatusCode::UNPROCESSABLE_ENTITY, m).into_response(),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m).into_response(),
        }
    }
}

/// Check that `code`'s universe can be loaded now, then start the load.
///
/// The checks run in the order a person fixing them would want: does the
/// connection exist, is a load already running, are its credentials usable,
/// can this broker be loaded at all, is it the connection the OMS routes to.
/// Nothing is written unless all pass.
pub async fn start_load(pool: &PgPool, master: Option<&MasterKey>, code: &str) -> Result<Uuid, LoadRefusal> {
    let broker = check_loadable(pool, master, code, JobKind::UniverseLoad).await?;
    super::universe::spawn_universe_load(pool.clone(), broker, "admin").await.map_err(refusal)
}

/// Check `code` can be enriched now, then start an `enrichment` job: the same
/// checks as a universe load.
pub async fn start_enrichment(pool: &PgPool, master: Option<&MasterKey>, code: &str) -> Result<Uuid, LoadRefusal> {
    let broker = check_loadable(pool, master, code, JobKind::Enrichment).await?;
    super::universe::spawn_enrichment(pool.clone(), broker, "admin").await.map_err(refusal)
}

fn refusal(e: CreateError) -> LoadRefusal {
    match e {
        CreateError::AlreadyRunning(id) => LoadRefusal::AlreadyRunning(id),
        CreateError::Db(e) => LoadRefusal::Internal(e.to_string()),
    }
}

async fn check_loadable(pool: &PgPool, master: Option<&MasterKey>, code: &str, kind: JobKind) -> Result<Broker, LoadRefusal> {
    let connections = crate::credentials::load_brokers(pool, master)
        .await
        .map_err(|e| LoadRefusal::Internal(format!("could not read broker connections: {e}")))?;
    let conn = connections.into_iter().find(|c| c.code == code).ok_or(LoadRefusal::NotFound)?;

    // A load and an enrichment write the same rows: either one active blocks both.
    let latest_job = if kind.writes_universe() {
        super::latest_universe_job(pool, code).await
    } else {
        latest(pool, kind, code).await
    };
    if let Some(job) = latest_job.map_err(|e| LoadRefusal::Internal(e.to_string()))? {
        if matches!(job.status, JobStatus::Queued | JobStatus::Running) {
            return Err(LoadRefusal::AlreadyRunning(job.id));
        }
    }

    match &conn.credentials {
        CredentialState::Configured(_) => {}
        CredentialState::Unconfigured => {
            return Err(LoadRefusal::Unprocessable(format!("{code}: add credentials first")))
        }
        CredentialState::Error(e) => {
            return Err(LoadRefusal::Unprocessable(format!("{code}: credentials unusable: {e}")))
        }
    }
    let broker = match conn.kind.as_str() {
        "ALPACA" => Broker::Alpaca,
        "BINANCE" => Broker::Binance,
        other => return Err(LoadRefusal::Unprocessable(format!("{other} has no instrument universe to load"))),
    };
    // A load always runs with the broker's active connection (ALPACA_ENV /
    // BINANCE_ENV decide which); loading another connection's universe would
    // silently use the active one's credentials.
    let active = broker.connection_code();
    if active != code {
        return Err(LoadRefusal::Unprocessable(format!(
            "{code} is not the active {} connection ({active} is)",
            broker.code()
        )));
    }

    Ok(broker)
}

fn master_key() -> Result<Option<MasterKey>, LoadRefusal> {
    match crate::config::master_key(crate::config::load()) {
        Some(Ok(k)) => Ok(Some(k)),
        Some(Err(e)) => Err(LoadRefusal::Internal(format!("invalid master key: {e}"))),
        None => Ok(None),
    }
}

/// Get a broker connection's instrument universe status
#[utoipa::path(
    get, path = "/admin/broker-connections/{code}/universe", tag = "admin",
    operation_id = "get_connection_universe",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 200, description = "OK", body = UniverseStatus),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_connection_universe(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<UniverseStatus>, LoadRefusal> {
    let pool = state.pool();
    let internal = |e: sqlx::Error| LoadRefusal::Internal(e.to_string());
    let broker_code: String = sqlx::query_scalar("SELECT broker_code FROM broker_connection WHERE code = $1")
        .bind(&code)
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .ok_or(LoadRefusal::NotFound)?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM broker_instrument WHERE broker_code = $1")
        .bind(&broker_code)
        .fetch_one(pool)
        .await
        .map_err(internal)?;
    let last_job = super::latest_universe_job(pool, &code).await.map_err(internal)?;
    let (state, reason) = derive_universe_state(last_job.as_ref(), count);
    Ok(Json(UniverseStatus { state, count, reason, last_job }))
}

/// `?limit=` for the job list: 50 by default, 1 to 200.
pub fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 200)
}

pub async fn list_jobs_page(pool: &PgPool, limit: Option<i64>) -> Result<Vec<JobRecord>, sqlx::Error> {
    super::list_recent(pool, clamp_limit(limit)).await
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct JobDetail {
    pub job: JobRecord,
    /// The job's own log, oldest line first (the newest 500 are kept).
    pub log: Vec<JobLogLine>,
}

pub async fn job_detail(pool: &PgPool, id: Uuid) -> Result<Option<JobDetail>, sqlx::Error> {
    Ok(super::get(pool, id).await?.map(|(job, log)| JobDetail { job, log }))
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct JobListQuery {
    /// How many, newest first: default 50, at most 200.
    pub limit: Option<i64>,
}

/// List recent background jobs
#[utoipa::path(
    get, path = "/admin/jobs", tag = "admin",
    operation_id = "list_jobs",
    params(JobListQuery),
    responses((status = 200, description = "Newest first", body = [JobRecord])),
    security(("bearer_token" = []))
)]
pub async fn list_jobs(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<JobListQuery>,
) -> Result<Json<Vec<JobRecord>>, LoadRefusal> {
    list_jobs_page(state.pool(), q.limit).await.map(Json).map_err(|e| LoadRefusal::Internal(e.to_string()))
}

/// Get a background job and its log
#[utoipa::path(
    get, path = "/admin/jobs/{id}", tag = "admin",
    operation_id = "get_job",
    params(("id" = Uuid, Path, description = "Job id")),
    responses(
        (status = 200, description = "OK", body = JobDetail),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_job(State(state): State<AppState>, Path(id): Path<Uuid>) -> Response {
    match job_detail(state.pool(), id).await {
        Ok(Some(detail)) => Json(detail).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "job not found").into_response(),
        Err(e) => LoadRefusal::Internal(e.to_string()).into_response(),
    }
}

/// Start an expiry sweep as a job: derive missing expiry instants, then retire
/// what has passed (`expiry::run_once`, the same pass the hourly task runs).
pub async fn start_expiry_sweep(pool: &PgPool, started_by: &str) -> Result<Uuid, LoadRefusal> {
    let id = super::create(pool, JobKind::ExpirySweep, None, started_by, json!({})).await.map_err(refusal)?;
    let handle = super::JobHandle::new(pool.clone(), id);
    tokio::spawn(async move {
        let _ = handle.start().await;
        // In a task of its own so a panic is seen here and still ends the job.
        let pool = handle.pool().clone();
        let outcome = match tokio::spawn(async move { crate::expiry::run_once(&pool).await }).await {
            Ok(Ok((recomputed, expired))) => Ok(json!({ "recomputed": recomputed, "expired": expired })),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("the sweep crashed: {e}")),
        };
        let recorded = match outcome {
            Ok(result) => handle.succeed(result).await,
            Err(e) => handle.fail(&e).await,
        };
        if let Err(e) = recorded {
            tracing::error!("expiry sweep job {}: could not record its outcome: {e}", handle.id);
        }
    });
    Ok(id)
}

/// Enrich a broker connection's instruments with FIGIs
#[utoipa::path(
    post, path = "/admin/broker-connections/{code}/universe/enrich", tag = "admin",
    operation_id = "enrich_connection_universe",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 202, description = "Started; follow it at /admin/jobs/{job_id}", body = JobStarted),
        (status = 404, description = "Not found"),
        (status = 409, description = "Enrichment for this connection is already running; its job_id is in the body"),
        (status = 422, description = "No usable credentials, a broker without a catalog, or not the active connection"),
    ),
    security(("bearer_token" = []))
)]
pub async fn enrich_connection_universe(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<(StatusCode, Json<JobStarted>), LoadRefusal> {
    let master = master_key()?;
    let job_id = start_enrichment(state.pool(), master.as_ref(), &code).await?;
    Ok((StatusCode::ACCEPTED, Json(JobStarted { job_id })))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct JobStarted {
    pub job_id: Uuid,
}

/// Load or reload a broker connection's instrument universe
#[utoipa::path(
    post, path = "/admin/broker-connections/{code}/universe/load", tag = "admin",
    operation_id = "load_connection_universe",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 202, description = "Started; follow it at /admin/jobs/{job_id}", body = JobStarted),
        (status = 404, description = "Not found"),
        (status = 409, description = "A load for this connection is already running; its job_id is in the body"),
        (status = 422, description = "No usable credentials, a broker without a catalog, or not the active connection"),
    ),
    security(("bearer_token" = []))
)]
pub async fn load_connection_universe(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<(StatusCode, Json<JobStarted>), LoadRefusal> {
    let master = master_key()?;
    let job_id = start_load(state.pool(), master.as_ref(), &code).await?;
    Ok((StatusCode::ACCEPTED, Json(JobStarted { job_id })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(status: JobStatus, result: Option<serde_json::Value>, error: Option<&str>) -> JobRecord {
        JobRecord {
            id: uuid::Uuid::nil(),
            kind: "universe_load".into(),
            subject: Some("alpaca-paper".into()),
            started_by: "test".into(),
            params: json!({}),
            status,
            progress_done: None,
            progress_total: None,
            result,
            error: error.map(str::to_string),
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn a_running_load_reads_as_loading() {
        assert_eq!(derive_universe_state(Some(&job(JobStatus::Running, None, None)), 10).0, UniverseState::Loading);
        assert_eq!(derive_universe_state(Some(&job(JobStatus::Queued, None, None)), 0).0, UniverseState::Loading);
    }

    #[test]
    fn a_failed_load_reads_as_failed() {
        let (state, reason) = derive_universe_state(Some(&job(JobStatus::Failed, None, Some("bad key"))), 0);
        assert_eq!(state, UniverseState::Failed);
        assert_eq!(reason.as_deref(), Some("bad key"));
    }

    #[test]
    fn an_interrupted_load_reads_as_partial() {
        let (state, reason) = derive_universe_state(Some(&job(JobStatus::Interrupted, None, None)), 9880);
        assert_eq!(state, UniverseState::Partial);
        assert_eq!(reason.as_deref(), Some("interrupted"));
    }

    #[test]
    fn skipped_instruments_read_as_partial_with_the_reason() {
        let result = json!({"fetched": 14414, "mapped": 14404, "skipped": 10, "skipped_reasons": ["unknown venue TXSE"]});
        let (state, reason) = derive_universe_state(Some(&job(JobStatus::Succeeded, Some(result), None)), 14404);
        assert_eq!(state, UniverseState::Partial);
        assert_eq!(reason.as_deref(), Some("10 skipped: unknown venue TXSE"));
    }

    #[test]
    fn a_succeeded_load_reads_as_loaded() {
        let result = json!({"fetched": 14404, "mapped": 14404, "skipped": 0, "skipped_reasons": []});
        let (state, reason) = derive_universe_state(Some(&job(JobStatus::Succeeded, Some(result), None)), 14404);
        assert_eq!(state, UniverseState::Loaded);
        assert_eq!(reason, None);
    }

    #[test]
    fn a_cli_loaded_universe_reads_as_loaded() {
        assert_eq!(derive_universe_state(None, 14404).0, UniverseState::Loaded);
    }

    #[test]
    fn no_job_and_no_instruments_reads_as_not_loaded() {
        assert_eq!(derive_universe_state(None, 0).0, UniverseState::NotLoaded);
        let empty = json!({"fetched": 0, "mapped": 0, "skipped": 0, "skipped_reasons": []});
        assert_eq!(derive_universe_state(Some(&job(JobStatus::Succeeded, Some(empty), None)), 0).0, UniverseState::NotLoaded);
    }

    // --- start_load: the checks behind POST …/universe/load ------------------

    async fn test_pool() -> sqlx::PgPool {
        dotenvy::dotenv().ok();
        let cfg = crate::setup::database::config::resolve(Default::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// A broker connection row with no credentials stored. `(broker_code,
    /// environment)` is unique and the real ALPACA/PAPER row exists, so the
    /// test row gets a broker code of its own.
    async fn bare_connection(pool: &sqlx::PgPool) -> String {
        let code = format!("test-{}", uuid::Uuid::new_v4().simple());
        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $1, 'PAPER', 'ACTIVE')")
            .bind(&code)
            .execute(pool)
            .await
            .unwrap();
        code
    }

    async fn drop_connection(pool: &sqlx::PgPool, code: &str) {
        sqlx::query("DELETE FROM job WHERE subject = $1").bind(code).execute(pool).await.unwrap();
        sqlx::query("DELETE FROM broker_connection WHERE code = $1").bind(code).execute(pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn load_endpoint_404s_an_unknown_connection() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let master = crate::config::master_key(crate::config::load()).and_then(Result::ok);
        let err = start_load(&pool, master.as_ref(), "no-such-connection").await.unwrap_err();
        assert!(matches!(err, LoadRefusal::NotFound), "{err:?}");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn load_endpoint_refuses_unconfigured_credentials() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let master = crate::config::master_key(crate::config::load()).and_then(Result::ok);
        let code = bare_connection(&pool).await;
        let err = start_load(&pool, master.as_ref(), &code).await.unwrap_err();
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM job WHERE subject = $1")
            .bind(&code)
            .fetch_one(&pool)
            .await
            .unwrap();
        drop_connection(&pool, &code).await;
        assert!(matches!(&err, LoadRefusal::Unprocessable(m) if m.contains("add credentials first")), "{err:?}");
        assert_eq!(jobs, 0, "no job row is written for a refused load");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn load_endpoint_refuses_while_one_is_running() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let master = crate::config::master_key(crate::config::load()).and_then(Result::ok);
        let code = bare_connection(&pool).await;
        let active = super::super::create(&pool, JobKind::UniverseLoad, Some(&code), "test", json!({})).await.unwrap();
        let err = start_load(&pool, master.as_ref(), &code).await.unwrap_err();
        drop_connection(&pool, &code).await;
        match err {
            LoadRefusal::AlreadyRunning(id) => assert_eq!(id, active),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    // --- job list / detail, sweep, enrichment ---------------------------------

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn list_jobs_returns_newest_first_and_honours_limit() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let s = format!("test-{}", uuid::Uuid::new_v4().simple());
        let mut ids = Vec::new();
        for _ in 0..3 {
            let id = super::super::create(&pool, JobKind::Enrichment, Some(&s), "test", json!({})).await.unwrap();
            super::super::JobHandle::new(pool.clone(), id).succeed(json!({})).await.unwrap();
            ids.push(id);
        }
        // Other tests create jobs concurrently in the same database, so the
        // global list is checked for its shape, and this test's own jobs for
        // their order within it.
        let two = list_jobs_page(&pool, Some(2)).await.unwrap();
        let all = list_jobs_page(&pool, Some(200)).await.unwrap();
        sqlx::query("DELETE FROM job WHERE subject = $1").bind(&s).execute(&pool).await.unwrap();
        assert_eq!(two.len(), 2, "limit honoured");
        assert!(all.windows(2).all(|w| w[0].created_at >= w[1].created_at), "newest first");
        let mine: Vec<_> = all.iter().filter(|j| j.subject.as_deref() == Some(s.as_str())).map(|j| j.id).collect();
        assert_eq!(mine, [ids[2], ids[1], ids[0]]);
    }

    #[test]
    fn the_job_list_limit_defaults_to_50_and_caps_at_200() {
        assert_eq!(clamp_limit(None), 50);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1000)), 200);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn get_job_returns_its_log_oldest_first() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let s = format!("test-{}", uuid::Uuid::new_v4().simple());
        let id = super::super::create(&pool, JobKind::Enrichment, Some(&s), "test", json!({})).await.unwrap();
        let h = super::super::JobHandle::new(pool.clone(), id);
        h.log("info", "first").await.unwrap();
        h.log("warn", "second").await.unwrap();
        let detail = job_detail(&pool, id).await.unwrap();
        sqlx::query("DELETE FROM job WHERE subject = $1").bind(&s).execute(&pool).await.unwrap();
        let detail = detail.expect("job exists");
        assert_eq!(detail.job.id, id);
        let messages: Vec<&str> = detail.log.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(messages, ["first", "second"]);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn get_job_404s_an_unknown_id() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        assert!(job_detail(&pool, uuid::Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn expiry_sweep_endpoint_answers_202_with_a_job() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let id = start_expiry_sweep(&pool, "test").await.unwrap();
        // The sweep runs in the background; it is quick.
        let mut job = None;
        for _ in 0..50 {
            let j = super::super::get(&pool, id).await.unwrap().unwrap().0;
            if j.status != JobStatus::Queued && j.status != JobStatus::Running {
                job = Some(j);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        sqlx::query("DELETE FROM job WHERE id = $1").bind(id).execute(&pool).await.unwrap();
        let job = job.expect("sweep finished");
        assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
        assert_eq!(job.kind, "expiry_sweep");
        let result = job.result.unwrap();
        assert!(result["recomputed"].is_u64() && result["expired"].is_u64(), "{result}");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn enrich_endpoint_refuses_unconfigured_credentials() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let master = crate::config::master_key(crate::config::load()).and_then(Result::ok);
        let code = bare_connection(&pool).await;
        let err = start_enrichment(&pool, master.as_ref(), &code).await.unwrap_err();
        drop_connection(&pool, &code).await;
        assert!(matches!(&err, LoadRefusal::Unprocessable(m) if m.contains("add credentials first")), "{err:?}");
    }
}

