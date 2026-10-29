//! A broker connection's instrument universe load, run as a job.
//!
//! The load itself is `setup::brokers::load_universe` — the same code the
//! `oms setup sync-broker` CLI runs. This module records it on a job row:
//! status, progress, its own log lines and the final report.

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::{create, CreateError, JobHandle, JobKind};
#[cfg(test)]
use super::{get, JobStatus};
use crate::adapters::InstrumentProvider;
use crate::setup::brokers::{self, Broker, LoadOptions, LoadProgress};

/// Cockpit and boot loads: base instruments only, no OpenFIGI enrichment (that
/// is its own job), and a load that skips some instruments still completes —
/// it reports them, and the connection shows "partial" with the reason.
/// Upper bound on one load, so a broker call that never returns cannot leave
/// its job "running" — which would also refuse every retry for the connection.
/// Generous: an enrichment pass through a rate-limited OpenFIGI is slow.
pub const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn default_options() -> LoadOptions {
    LoadOptions { no_enrich: true, allow_skips: true, underlyings: Vec::new() }
}

/// Record a universe load for `broker`'s connection and start it in the
/// background. Returns the job id at once; a second load while one is active is
/// refused with [`CreateError::AlreadyRunning`].
///
/// The load runs on a dedicated thread with its own current-thread runtime, as
/// the boot-time sync always has: the sync path threads non-`Send` boxed errors
/// across its awaits, which `tokio::spawn` on the server runtime would reject.
pub async fn spawn_universe_load(pool: PgPool, broker: Broker, started_by: &str) -> Result<Uuid, CreateError> {
    spawn_load(pool, broker, started_by, JobKind::UniverseLoad, default_options()).await
}

/// Look up FIGIs for `broker`'s instruments through OpenFIGI, as an
/// `enrichment` job: the same load with enrichment on. It re-fetches the
/// broker's list (seconds) because the enricher works on the broker's own
/// instrument definitions; the slow part is OpenFIGI, rate-limited without
/// `OPENFIGI_API_KEY`.
pub async fn spawn_enrichment(pool: PgPool, broker: Broker, started_by: &str) -> Result<Uuid, CreateError> {
    let opts = LoadOptions { no_enrich: false, ..default_options() };
    spawn_load(pool, broker, started_by, JobKind::Enrichment, opts).await
}

async fn spawn_load(
    pool: PgPool,
    broker: Broker,
    started_by: &str,
    kind: JobKind,
    opts: LoadOptions,
) -> Result<Uuid, CreateError> {
    let params = serde_json::json!({ "broker": broker.code(), "no_enrich": opts.no_enrich });
    let id = create(&pool, kind, Some(&broker.connection_code()), started_by, params).await?;
    let handle = JobHandle::new(pool.clone(), id);
    // Kept to record a failure from here if the load's own runtime cannot start.
    let server = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("universe load {id}: could not start a runtime: {e}");
                let message = format!("could not start: {e}");
                server.spawn(async move { record(&handle, Err(message)).await });
                return;
            }
        };
        rt.block_on(async move {
            match brokers::provider_for(&pool, broker).await {
                Ok(provider) => run_job(&handle, provider.as_ref(), broker, &opts, LOAD_TIMEOUT).await,
                Err(e) => record(&handle, Err(e.to_string())).await,
            }
        });
    });
    Ok(id)
}

/// Run one universe load against `provider` and record it on `handle`'s job:
/// running → progress and log lines as it goes → succeeded with the report, or
/// failed with the error.
///
/// Every way the load can end is recorded — an error, a panic inside the
/// adapter, or running past `timeout` — because a job left "running" also
/// refuses every retry for its connection until the OMS restarts.
pub async fn run_job(
    handle: &JobHandle,
    provider: &dyn InstrumentProvider,
    broker: Broker,
    opts: &LoadOptions,
    timeout: std::time::Duration,
) {
    use futures_util::FutureExt;

    if let Err(e) = handle.start().await {
        error!("universe load {}: could not mark running: {e}", handle.id);
    }

    // `LoadProgress` is synchronous; the job row is written asynchronously. A
    // channel to one writer task keeps the writes in the order they were made —
    // progress never goes backwards and log lines keep their sequence.
    let (tx, mut rx) = mpsc::unbounded_channel::<Update>();
    let writer_handle = handle.clone();
    let writer = tokio::spawn(async move {
        while let Some(update) = rx.recv().await {
            let written = match &update {
                Update::Progress(done, total) => writer_handle.progress(*done, Some(*total)).await,
                Update::Log(level, message) => writer_handle.log(level, message).await,
            };
            if let Err(e) = written {
                warn!("universe load {}: could not record progress: {e}", writer_handle.id);
            }
        }
    });

    let progress = ChannelProgress(tx);
    let load = tokio::time::timeout(timeout, brokers::load_universe(handle.pool(), broker, provider, opts, &progress));
    let outcome = match std::panic::AssertUnwindSafe(load).catch_unwind().await {
        Ok(Ok(Ok(report))) => {
            info!("universe load {}: {} fetched, {} mapped", handle.id, report.fetched, report.mapped);
            Ok(serde_json::to_value(&report).unwrap_or_default())
        }
        Ok(Ok(Err(e))) => Err(e.to_string()),
        Ok(Err(_elapsed)) => Err(format!("timed out after {} minutes", timeout.as_secs() / 60)),
        Err(panic) => Err(format!("the load crashed: {}", panic_message(&panic))),
    };
    // Dropping the only sender lets the writer drain what is queued and exit.
    drop(progress);
    let _ = writer.await;

    if let Err(e) = &outcome {
        warn!("universe load {} failed: {e}", handle.id);
    }
    record(handle, outcome).await;
}

/// Write a job's final status, retrying once: a job left "running" by one
/// failed write blocks every retry for its connection until a restart.
async fn record(handle: &JobHandle, outcome: Result<serde_json::Value, String>) {
    for attempt in 0..2 {
        let written = match &outcome {
            Ok(report) => handle.succeed(report.clone()).await,
            Err(e) => handle.fail(e).await,
        };
        match written {
            Ok(()) => return,
            Err(e) if attempt == 0 => {
                warn!("universe load {}: could not record its outcome, retrying: {e}", handle.id);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => error!("universe load {}: could not record its outcome: {e}", handle.id),
        }
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_string())
}

enum Update {
    Progress(i64, i64),
    Log(String, String),
}

struct ChannelProgress(mpsc::UnboundedSender<Update>);

impl LoadProgress for ChannelProgress {
    fn progress(&self, done: i64, total: i64) {
        let _ = self.0.send(Update::Progress(done, total));
    }
    fn log(&self, level: &str, message: &str) {
        let _ = self.0.send(Update::Log(level.to_string(), message.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{BrokerError, BrokerInstrument, InstrumentProvider};
    use crate::setup::brokers::{Broker, LoadOptions};
    use dataprovider::InstrumentDef;

    struct FakeProvider(Result<Vec<(String, String)>, String>);

    #[async_trait::async_trait]
    impl InstrumentProvider for FakeProvider {
        async fn list_instruments(&self, _u: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
            match &self.0 {
                Ok(list) => Ok(list.iter().map(|(s, v)| fake(s, v)).collect()),
                Err(e) => Err(BrokerError::BrokerRejected(e.clone())),
            }
        }
    }

    /// A broker call that panics (an adapter `unwrap` on an unexpected reply).
    struct PanickingProvider;

    #[async_trait::async_trait]
    impl InstrumentProvider for PanickingProvider {
        async fn list_instruments(&self, _u: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
            panic!("unexpected catalog reply");
        }
    }

    /// A broker call that never returns.
    struct HangingProvider;

    #[async_trait::async_trait]
    impl InstrumentProvider for HangingProvider {
        async fn list_instruments(&self, _u: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
            std::future::pending().await
        }
    }

    const LONG: std::time::Duration = std::time::Duration::from_secs(600);

    fn fake(symbol: &str, venue: &str) -> BrokerInstrument {
        BrokerInstrument {
            definition: InstrumentDef {
                symbol: symbol.to_string(),
                venue: venue.to_string(),
                currency: "USD".to_string(),
                asset_class: "EQUITY".to_string(),
                instrument_class: "SPOT".to_string(),
                name: Some(format!("{symbol} test instrument")),
                price_precision: 2,
                price_increment: 0.01,
                size_increment: 1.0,
                lot_size: None,
                contract_size: 1.0,
                native_id: None,
                provider_exchange: None,
                derivative: None,
                identifiers: Default::default(),
            },
            broker_symbol: symbol.to_string(),
            broker_exchange: None,
            is_tradeable: true,
            min_quantity: None,
            max_quantity: None,
            min_notional: None,
            max_notional: None,
        }
    }

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

    fn opts() -> LoadOptions {
        LoadOptions { no_enrich: true, allow_skips: true, underlyings: Vec::new() }
    }

    async fn new_job(pool: &sqlx::PgPool, subject: &str) -> JobHandle {
        let id = create(pool, JobKind::UniverseLoad, Some(subject), "test", serde_json::json!({})).await.unwrap();
        JobHandle::new(pool.clone(), id)
    }

    async fn cleanup(pool: &sqlx::PgPool, subject: &str, prefix: &str) {
        let like = format!("{prefix}%");
        sqlx::query("DELETE FROM job WHERE subject = $1").bind(subject).execute(pool).await.unwrap();
        sqlx::query(
            "DELETE FROM broker_instrument WHERE instrument_id IN (SELECT id FROM instrument WHERE symbol LIKE $1)",
        )
        .bind(&like)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM instrument WHERE symbol LIKE $1").bind(&like).execute(pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_universe_load_job_succeeds_with_its_report() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let tag = uuid::Uuid::new_v4().simple().to_string()[..6].to_uppercase();
        let (subject, prefix) = (format!("test-{tag}"), format!("ZZUJ{tag}"));
        let provider = FakeProvider(Ok(vec![
            (format!("{prefix}0"), "XNAS".to_string()),
            (format!("{prefix}1"), "XNAS".to_string()),
        ]));
        let h = new_job(&pool, &subject).await;

        run_job(&h, &provider, Broker::Alpaca, &opts(), LONG).await;

        let (job, log) = get(&pool, h.id).await.unwrap().unwrap();
        cleanup(&pool, &subject, &prefix).await;
        assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
        assert_eq!(job.result.as_ref().unwrap()["fetched"], 2);
        assert_eq!((job.progress_done, job.progress_total), (Some(2), Some(2)));
        assert!(!log.is_empty(), "the load's own log lines are kept on the job");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_failing_provider_fails_the_job_with_its_error() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let tag = uuid::Uuid::new_v4().simple().to_string()[..6].to_uppercase();
        let (subject, prefix) = (format!("test-{tag}"), format!("ZZUJ{tag}"));
        let provider = FakeProvider(Err("401 unauthorized".to_string()));
        let h = new_job(&pool, &subject).await;

        run_job(&h, &provider, Broker::Alpaca, &opts(), LONG).await;

        let (job, _) = get(&pool, h.id).await.unwrap().unwrap();
        cleanup(&pool, &subject, &prefix).await;
        assert_eq!(job.status, JobStatus::Failed);
        assert!(job.error.as_deref().unwrap_or("").contains("401 unauthorized"), "{:?}", job.error);
    }

    /// A panic in the load must still end the job, or the row stays "running"
    /// and the one-active-load index refuses every retry until a restart.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_panicking_provider_fails_the_job() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let subject = format!("test-{}", uuid::Uuid::new_v4().simple());
        let h = new_job(&pool, &subject).await;

        run_job(&h, &PanickingProvider, Broker::Alpaca, &opts(), LONG).await;

        let (job, _) = get(&pool, h.id).await.unwrap().unwrap();
        cleanup(&pool, &subject, "ZZNOTHING").await;
        assert_eq!(job.status, JobStatus::Failed);
        assert!(job.error.as_deref().unwrap_or("").contains("unexpected catalog reply"), "{:?}", job.error);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_hanging_provider_fails_the_job_on_timeout() {
        let _jobs = crate::jobs::test_lock::shared().await;
        let pool = test_pool().await;
        let subject = format!("test-{}", uuid::Uuid::new_v4().simple());
        let h = new_job(&pool, &subject).await;

        run_job(&h, &HangingProvider, Broker::Alpaca, &opts(), std::time::Duration::from_millis(300)).await;

        let (job, _) = get(&pool, h.id).await.unwrap().unwrap();
        cleanup(&pool, &subject, "ZZNOTHING").await;
        assert_eq!(job.status, JobStatus::Failed);
        assert!(job.error.as_deref().unwrap_or("").contains("timed out"), "{:?}", job.error);
    }
}

