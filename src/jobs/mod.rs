//! Background jobs: persisted records of one-off work and its outcome.
//!
//! A job is work with a beginning and an end — a broker's instrument universe
//! load, enrichment, a symbology backfill, an expiry sweep that changed
//! something. Each is a row in `oms.job` so the cockpit can show what is
//! running, how it ended and why, across restarts: [`mark_interrupted`] runs at
//! boot and turns anything still `queued`/`running` into `interrupted`.
//!
//! Always-on services (execution streams, price feeds, pollers) are not jobs.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub mod api;
pub mod universe;

/// Newest log lines kept per job; older ones are dropped as new ones arrive,
/// so a job that logs per batch over 14,000 symbols cannot grow without bound.
pub const LOG_CAP: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    UniverseLoad,
    Enrichment,
    SymbologyBackfill,
    ExpirySweep,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UniverseLoad => "universe_load",
            Self::Enrichment => "enrichment",
            Self::SymbologyBackfill => "symbology_backfill",
            Self::ExpirySweep => "expiry_sweep",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

impl JobStatus {
    fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            // The CHECK constraint admits nothing else.
            _ => Self::Interrupted,
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct JobRecord {
    pub id: Uuid,
    /// `universe_load`, `enrichment`, `symbology_backfill` or `expiry_sweep`.
    pub kind: String,
    /// What the job is about, e.g. a broker connection code.
    pub subject: Option<String>,
    /// `system` for work the OMS started itself, else the admin action.
    pub started_by: String,
    pub params: Value,
    pub status: JobStatus,
    pub progress_done: Option<i64>,
    pub progress_total: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for JobRecord {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            subject: row.try_get("subject")?,
            started_by: row.try_get("started_by")?,
            params: row.try_get("params")?,
            status: JobStatus::parse(row.try_get::<&str, _>("status")?),
            progress_done: row.try_get("progress_done")?,
            progress_total: row.try_get("progress_total")?,
            result: row.try_get("result")?,
            error: row.try_get("error")?,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct JobLogLine {
    pub at: DateTime<Utc>,
    pub level: String,
    pub message: String,
}

#[derive(Debug)]
pub enum CreateError {
    /// A job of this kind is already queued or running for this subject; the
    /// id is that job's.
    AlreadyRunning(Uuid),
    Db(sqlx::Error),
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(id) => write!(f, "a job for this subject is already running ({id})"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CreateError {}

const COLUMNS: &str = "id, kind, subject, started_by, params, status, progress_done, progress_total, \
                       result, error, created_at, started_at, finished_at";

/// Record a new job as `queued`. The partial unique index
/// `uq_job_active_per_subject` makes a second active job of the same kind for
/// the same subject fail atomically; that failure is reported as
/// [`CreateError::AlreadyRunning`] with the active job's id.
pub async fn create(
    pool: &PgPool,
    kind: JobKind,
    subject: Option<&str>,
    started_by: &str,
    params: Value,
) -> Result<Uuid, CreateError> {
    let id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO job (id, kind, subject, started_by, params, status) VALUES ($1, $2, $3, $4, $5, 'queued')",
    )
    .bind(id)
    .bind(kind.as_str())
    .bind(subject)
    .bind(started_by)
    .bind(params)
    .execute(pool)
    .await;
    match inserted {
        Ok(_) => Ok(id),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
            let active: Uuid = sqlx::query_scalar(
                "SELECT id FROM job WHERE kind = $1 AND subject IS NOT DISTINCT FROM $2 \
                 AND status IN ('queued', 'running')",
            )
            .bind(kind.as_str())
            .bind(subject)
            .fetch_one(pool)
            .await
            .map_err(CreateError::Db)?;
            Err(CreateError::AlreadyRunning(active))
        }
        Err(e) => Err(CreateError::Db(e)),
    }
}

/// Updates one job's row as its work progresses.
#[derive(Clone)]
pub struct JobHandle {
    pool: PgPool,
    pub id: Uuid,
}

impl JobHandle {
    pub fn new(pool: PgPool, id: Uuid) -> Self {
        Self { pool, id }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn start(&self) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE job SET status = 'running', started_at = now() WHERE id = $1")
            .bind(self.id)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// `total` of `None` leaves a previously reported total in place.
    pub async fn progress(&self, done: i64, total: Option<i64>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE job SET progress_done = $2, progress_total = COALESCE($3, progress_total) WHERE id = $1",
        )
        .bind(self.id)
        .bind(done)
        .bind(total)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Append a log line, then drop this job's lines beyond the newest [`LOG_CAP`].
    pub async fn log(&self, level: &str, message: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO job_log (job_id, level, message) VALUES ($1, $2, $3)")
            .bind(self.id)
            .bind(level)
            .bind(message)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "DELETE FROM job_log WHERE job_id = $1 AND id < \
             (SELECT min(id) FROM (SELECT id FROM job_log WHERE job_id = $1 ORDER BY id DESC LIMIT $2) newest)",
        )
        .bind(self.id)
        .bind(LOG_CAP)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    pub async fn succeed(&self, result: Value) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE job SET status = 'succeeded', result = $2, finished_at = now() WHERE id = $1")
            .bind(self.id)
            .bind(result)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    pub async fn fail(&self, error: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE job SET status = 'failed', error = $2, finished_at = now() WHERE id = $1")
            .bind(self.id)
            .bind(error)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }
}

/// At boot: anything left queued or running belonged to a process that is gone.
pub async fn mark_interrupted(pool: &PgPool) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE job SET status = 'interrupted', finished_at = now() WHERE status IN ('queued', 'running')",
    )
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

/// Newest first.
pub async fn list_recent(pool: &PgPool, limit: i64) -> Result<Vec<JobRecord>, sqlx::Error> {
    sqlx::query_as::<_, JobRecord>(&format!("SELECT {COLUMNS} FROM job ORDER BY created_at DESC LIMIT $1"))
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// A job and its log, oldest line first.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<(JobRecord, Vec<JobLogLine>)>, sqlx::Error> {
    let Some(job) = sqlx::query_as::<_, JobRecord>(&format!("SELECT {COLUMNS} FROM job WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let log = sqlx::query_as::<_, JobLogLine>(
        "SELECT at, level, message FROM job_log WHERE job_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Some((job, log)))
}

/// The most recent job of `kind` for `subject`, whatever its status.
pub async fn latest(pool: &PgPool, kind: JobKind, subject: &str) -> Result<Option<JobRecord>, sqlx::Error> {
    sqlx::query_as::<_, JobRecord>(&format!(
        "SELECT {COLUMNS} FROM job WHERE kind = $1 AND subject = $2 ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(kind.as_str())
    .bind(subject)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
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

    /// A subject no other test (or real job) uses, so tests never collide on
    /// the one-active-job-per-subject index.
    fn subject() -> String {
        format!("test-{}", uuid::Uuid::new_v4())
    }

    async fn cleanup(pool: &sqlx::PgPool, subject: &str) {
        sqlx::query("DELETE FROM job WHERE subject = $1").bind(subject).execute(pool).await.unwrap();
    }

    async fn record(pool: &sqlx::PgPool, id: uuid::Uuid) -> JobRecord {
        get(pool, id).await.unwrap().expect("job exists").0
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_job_runs_through_its_lifecycle() {
        let pool = test_pool().await;
        let s = subject();
        let id = create(&pool, JobKind::UniverseLoad, Some(&s), "test", json!({})).await.unwrap();
        assert_eq!(record(&pool, id).await.status, JobStatus::Queued);

        let h = JobHandle::new(pool.clone(), id);
        h.start().await.unwrap();
        let r = record(&pool, id).await;
        assert_eq!(r.status, JobStatus::Running);
        assert!(r.started_at.is_some());

        h.progress(3, Some(10)).await.unwrap();
        let r = record(&pool, id).await;
        assert_eq!((r.progress_done, r.progress_total), (Some(3), Some(10)));

        h.succeed(json!({"n": 3})).await.unwrap();
        let r = record(&pool, id).await;
        assert_eq!(r.status, JobStatus::Succeeded);
        assert!(r.finished_at.is_some());
        assert_eq!(r.result, Some(json!({"n": 3})));
        cleanup(&pool, &s).await;
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_second_active_universe_load_for_the_same_connection_is_refused() {
        let pool = test_pool().await;
        let s = subject();
        let first = create(&pool, JobKind::UniverseLoad, Some(&s), "test", json!({})).await.unwrap();
        match create(&pool, JobKind::UniverseLoad, Some(&s), "test", json!({})).await {
            Err(CreateError::AlreadyRunning(id)) => assert_eq!(id, first),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        JobHandle::new(pool.clone(), first).succeed(json!({})).await.unwrap();
        assert!(create(&pool, JobKind::UniverseLoad, Some(&s), "test", json!({})).await.is_ok());
        cleanup(&pool, &s).await;
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn jobs_left_running_are_marked_interrupted() {
        let pool = test_pool().await;
        let (a, b, c) = (subject(), subject(), subject());
        let running = create(&pool, JobKind::UniverseLoad, Some(&a), "test", json!({})).await.unwrap();
        JobHandle::new(pool.clone(), running).start().await.unwrap();
        let queued = create(&pool, JobKind::UniverseLoad, Some(&b), "test", json!({})).await.unwrap();
        let done = create(&pool, JobKind::UniverseLoad, Some(&c), "test", json!({})).await.unwrap();
        JobHandle::new(pool.clone(), done).succeed(json!({})).await.unwrap();

        assert!(mark_interrupted(&pool).await.unwrap() >= 2);
        for id in [running, queued] {
            let r = record(&pool, id).await;
            assert_eq!(r.status, JobStatus::Interrupted);
            assert!(r.finished_at.is_some());
        }
        assert_eq!(record(&pool, done).await.status, JobStatus::Succeeded);
        for s in [a, b, c] {
            cleanup(&pool, &s).await;
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_job_log_keeps_only_the_newest_500_lines() {
        let pool = test_pool().await;
        let s = subject();
        let id = create(&pool, JobKind::Enrichment, Some(&s), "test", json!({})).await.unwrap();
        let h = JobHandle::new(pool.clone(), id);
        for i in 0..510 {
            h.log("info", &i.to_string()).await.unwrap();
        }
        let (_, log) = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(log.len() as i64, LOG_CAP);
        assert_eq!(log[0].message, "10");
        assert_eq!(log.last().unwrap().message, "509");
        cleanup(&pool, &s).await;
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn failing_a_job_records_the_error() {
        let pool = test_pool().await;
        let s = subject();
        let id = create(&pool, JobKind::UniverseLoad, Some(&s), "test", json!({})).await.unwrap();
        JobHandle::new(pool.clone(), id).fail("bad key").await.unwrap();
        let r = record(&pool, id).await;
        assert_eq!(r.status, JobStatus::Failed);
        assert_eq!(r.error.as_deref(), Some("bad key"));
        cleanup(&pool, &s).await;
    }
}
