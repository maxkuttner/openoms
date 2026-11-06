//! Shared supervision for long-lived reconnecting streams.
//!
//! Every stream task — Alpaca and Binance order streams, the Databento OPRA marks
//! feed — wants the same outer loop: mark the health badge `connecting`, run one
//! session until it closes or errors, mark it `down`, wait, reconnect. Only the
//! session body differs. This module owns the loop; [`Session`] is the seam.
//!
//! The streams are deliberately *not* unified beyond this. Order streams emit
//! execution reports; the marks feed emits quotes. They share mechanics, not
//! purpose — a single trait over both would be the wrong abstraction.

use std::time::Instant;

use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::stream_health::StreamHandle;

pub type StreamResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// One connected session of a stream.
///
/// `run_once` returns when the connection ends: `Ok(())` for a clean close,
/// `Err` for a failure. Either way the supervisor reconnects — the distinction
/// only affects what gets logged and shown on the health badge.
#[async_trait::async_trait]
pub trait Session: Send {
    async fn run_once(&mut self) -> StreamResult;
}

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 30;

/// A session that stayed up at least this long is treated as healthy, so its next
/// failure restarts the backoff from the bottom. Without this, backoff only ever
/// grows for the life of the process: a stream that ran for hours and then dropped
/// once would wait the last capped delay before retrying, as if it were flapping.
const STABLE_AFTER_SECS: u64 = 60;

/// Run `session` forever, reconnecting with exponential backoff and keeping the
/// health badge current.
pub async fn supervise<S: Session>(name: &'static str, health: StreamHandle, mut session: S) -> ! {
    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        health.set_connecting();
        info!(stream = name, "connecting");

        let started = Instant::now();
        match session.run_once().await {
            Ok(()) => {
                health.set_down("stream closed");
                warn!(stream = name, "stream closed, reconnecting in {backoff_secs}s");
            }
            Err(e) => {
                health.set_down(e.to_string());
                warn!(stream = name, error = %e, "stream error, reconnecting in {backoff_secs}s");
            }
        }

        if started.elapsed() >= Duration::from_secs(STABLE_AFTER_SECS) {
            backoff_secs = INITIAL_BACKOFF_SECS;
        }

        sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backoff schedule, extracted so the growth/reset rule is testable without
    /// running a supervisor. Mirrors the arithmetic in `supervise`.
    fn next_backoff(current: u64, session_lasted: Duration) -> u64 {
        let base = if session_lasted >= Duration::from_secs(STABLE_AFTER_SECS) {
            INITIAL_BACKOFF_SECS
        } else {
            current
        };
        (base * 2).min(MAX_BACKOFF_SECS)
    }

    #[test]
    fn backoff_grows_while_flapping() {
        let brief = Duration::from_secs(0);
        let mut b = INITIAL_BACKOFF_SECS;
        b = next_backoff(b, brief);
        assert_eq!(b, 2);
        b = next_backoff(b, brief);
        assert_eq!(b, 4);
        b = next_backoff(b, brief);
        assert_eq!(b, 8);
    }

    #[test]
    fn backoff_caps() {
        let brief = Duration::from_secs(0);
        let mut b = INITIAL_BACKOFF_SECS;
        for _ in 0..12 {
            b = next_backoff(b, brief);
        }
        assert_eq!(b, MAX_BACKOFF_SECS);
    }

    /// The bug this module exists to fix: a long-lived session used to inherit the
    /// last capped delay, so a stream up for hours retried as if it were flapping.
    #[test]
    fn stable_session_resets_backoff() {
        let brief = Duration::from_secs(0);
        let mut b = INITIAL_BACKOFF_SECS;
        for _ in 0..10 {
            b = next_backoff(b, brief);
        }
        assert_eq!(b, MAX_BACKOFF_SECS, "should be capped after a flapping burst");

        let stable = Duration::from_secs(STABLE_AFTER_SECS);
        assert_eq!(next_backoff(b, stable), 2, "a healthy session restarts the schedule");
    }

    #[test]
    fn session_just_under_threshold_does_not_reset() {
        let almost = Duration::from_secs(STABLE_AFTER_SECS - 1);
        assert_eq!(next_backoff(16, almost), 32.min(MAX_BACKOFF_SECS));
    }
}
