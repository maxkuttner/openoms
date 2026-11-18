//! End-of-day sweep: once a day, pull each reconciled connection's executions
//! back over its `eod_lookback` and book what the intraday pulls missed.
//!
//! The sweep never moves the intraday cursors (see `ReconcilerHandle::sweep`).
//! Each run is recorded as a `system` job of kind `eod_sweep`, subject the
//! connection, its [`SweepReport`] as the result. A connection without a live
//! reconciler is skipped; the job looks it up per run (`reconciler::live`)
//! rather than holding a handle.

use std::str::FromStr;

use chrono::{DateTime, Duration, TimeZone, Utc};
use sqlx::PgPool;
use tracing::{error, info, warn};

use super::{record_outcome, JobKind};
use crate::reconciler::SweepReport;

/// UTC time of day the sweep runs.
const RUN_AT: (u32, u32) = (0, 15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookback {
    D1,
    W1,
    M1,
    Y1,
    Y5,
    Y10,
}

impl Lookback {
    pub fn duration(&self) -> Duration {
        Duration::days(match self {
            Self::D1 => 1,
            Self::W1 => 7,
            Self::M1 => 30,
            Self::Y1 => 365,
            Self::Y5 => 5 * 365,
            Self::Y10 => 10 * 365,
        })
    }
}

impl FromStr for Lookback {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "1d" => Self::D1,
            "1w" => Self::W1,
            "1m" => Self::M1,
            "1y" => Self::Y1,
            "5y" => Self::Y5,
            "10y" => Self::Y10,
            other => return Err(format!("unknown lookback {other:?}; one of 1d, 1w, 1m, 1y, 5y, 10y")),
        })
    }
}

/// The next 00:15 UTC strictly after `now`.
fn next_run(now: DateTime<Utc>) -> DateTime<Utc> {
    let today = Utc.from_utc_datetime(&now.date_naive().and_hms_opt(RUN_AT.0, RUN_AT.1, 0).unwrap());
    if today > now { today } else { today + Duration::days(1) }
}

/// Sweep one connection now: its own reconciler and its co-located follows',
/// as one report. `None` when it has no live reconciler.
pub async fn run_connection(pool: &PgPool, connection: &str, lookback: Lookback) -> Option<SweepReport> {
    let handles = crate::reconciler::live_all(connection);
    let mut report: Option<SweepReport> = None;
    for handle in handles {
        let one = handle.sweep(lookback.duration()).await;
        drop(handle);
        report = Some(match report {
            None => one,
            Some(sum) => sum.merge(one),
        });
    }
    let report = report?;
    info!(connection, ?report, "eod sweep");
    match serde_json::to_value(&report) {
        Ok(result) => {
            let error = failure(&report);
            if let Some(err) = &error {
                warn!(connection, error = %err, "eod sweep failed");
            }
            if let Err(e) = record_outcome(pool, JobKind::EodSweep, Some(connection), "system", result, error.as_deref()).await {
                error!(connection, error = %e, "could not record the eod sweep as a job");
            }
        }
        Err(e) => error!(connection, error = %e, "eod sweep report not serializable"),
    }
    Some(report)
}

/// A sweep that booked nothing and hit failures or errors failed; its error is
/// every error text (the report keeps them too). One that booked something
/// succeeded, with its errors in the report.
pub(crate) fn failure(report: &SweepReport) -> Option<String> {
    if report.booked > 0 || (report.failed == 0 && report.errors.is_empty()) {
        return None;
    }
    Some(if report.errors.is_empty() {
        format!("{} fills failed to ingest", report.failed)
    } else {
        report.errors.join("; ")
    })
}

/// Sweep every active connection that has a reconciler.
pub async fn run_once(pool: &PgPool) {
    let rows: Vec<(String, String)> = match sqlx::query_as("SELECT code, eod_lookback FROM broker_connection WHERE status = 'ACTIVE' ORDER BY code")
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "eod sweep: connections unreadable");
            return;
        }
    };
    for (code, lookback) in rows {
        match lookback.parse::<Lookback>() {
            Ok(l) => {
                run_connection(pool, &code, l).await;
            }
            Err(e) => warn!(connection = %code, error = %e, "eod sweep skipped"),
        }
    }
}

/// Background task: sweep daily at 00:15 UTC, forever.
pub async fn run(pool: PgPool) {
    loop {
        let wait = (next_run(Utc::now()) - Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(wait).await;
        run_once(&pool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eod_sweep_lookback_parses_all_six_values() {
        let want = [("1d", 1), ("1w", 7), ("1m", 30), ("1y", 365), ("5y", 5 * 365), ("10y", 10 * 365)];
        for (s, days) in want {
            assert_eq!(s.parse::<Lookback>().unwrap().duration(), chrono::Duration::days(days), "{s}");
        }
        assert!("2w".parse::<Lookback>().is_err());
        assert!("".parse::<Lookback>().is_err());
    }

    #[test]
    fn a_sweep_with_only_failures_is_a_failed_job_with_its_errors() {
        let at = Utc.with_ymd_and_hms(2026, 11, 18, 0, 15, 0).unwrap();
        let mut r = SweepReport { requested_from: at, obtained_from: at, booked: 0, duplicates: 3, breaks: 0, failed: 1, errors: vec!["a BTCUSDT: fill x: boom".into()] };
        assert_eq!(failure(&r).as_deref(), Some("a BTCUSDT: fill x: boom"));
        r.errors.clear();
        assert_eq!(failure(&r).as_deref(), Some("1 fills failed to ingest"));
        r.booked = 1;
        assert_eq!(failure(&r), None, "booked something: succeeded");
        r = SweepReport { booked: 0, failed: 0, errors: vec!["a: scopes: down".into()], ..r };
        assert!(failure(&r).is_some(), "a venue error with nothing booked fails");
        r.errors.clear();
        assert_eq!(failure(&r), None, "a clean empty sweep succeeds");
    }

    #[test]
    fn eod_sweep_runs_at_0015_utc() {
        let t = |h, m| Utc.with_ymd_and_hms(2026, 11, 18, h, m, 0).unwrap();
        assert_eq!(next_run(t(0, 10)), t(0, 15));
        assert_eq!(next_run(t(0, 15)), t(0, 15) + Duration::days(1));
        assert_eq!(next_run(t(13, 0)), t(0, 15) + Duration::days(1));
    }
}
