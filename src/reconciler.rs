//! One reconciler per venue connection: the single place that moves its venue
//! events into the engine and fills the gaps the stream leaves.
//!
//! - **Stream:** events from the connection's [`VenueStream`] sink are forwarded
//!   to the engine in order. `Connected` triggers a pull instead.
//! - **Intraday pull:** on `Connected`, on a timer (`broker_connection.intraday_poll_secs`,
//!   else `hints().intraday_secs`) and on [`ReconcilerHandle::pull_now`]. For each
//!   account of the connection × `scopes()`, fills are paged from the stored
//!   `recon_cursor`. Each fill is ingested and **awaited**; the cursor advances only
//!   once a whole page is booked, so a failure re-pulls that page next time
//!   (ingest dedupes on exec id). Then the open-order snapshot is applied, and an
//!   OMS-working order missing from it is closed with the status the venue gives
//!   for it (the port of `recon_orders`' `ResolveTerminal`). That close books no
//!   fills: ingest flags a fill gap and the gap triggers another pull.
//! - **Triggers coalesce:** one pull runs at a time; any number of triggers while
//!   it runs queue exactly one rerun.
//! - **Cursors:** a missing cursor is created empty, stamped "now", and history
//!   is skipped (history comes only from the EOD sweep). An empty cursor pulls
//!   with `from = None, since = updated_at - 60s`; a non-empty returned cursor is
//!   stored, an empty one leaves the row (and its stamp) untouched.
//!
//! **Double-booking guard (transition):** a connector still on the old
//! `ExecEvent` session path books its OMS orders' fills without writing
//! `venue_fill`, so pulling the same fills through ingest would book them twice.
//! A reconciler is therefore started only for a connection whose `Connected`
//! has both `source` and `venue_stream` (see `reload::build_registry`); no
//! connector sets them yet. Each connector switches to venue events only, and
//! sets both, in one step.
//!
//! The instrument provider (from `Connector::universe_provider`) rides along on
//! every `GatewayMsg::Venue` this sends, so the engine keeps no per-connection map.

// `pull_now`, `sweep` and the plain `spawn` are for the EOD sweep (Task 6) and
// the connectors moved onto venue events; nothing calls them yet.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gateway::adapter::{BrokerAdapter, InstrumentProvider};
use gateway::contract::{BrokerOrderOutcome, EngineHandle, IngestOutcome};
use gateway::venue_event::{Cursor, ExecutionSource, VenueEvent, VenueOrderStatus, VenueOrderUpdate};
use sqlx::PgPool;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// How far before an empty cursor's stamp a pull reaches back, against clock
/// skew between us and the venue; overlap is free (ingest dedupes).
const SINCE_MARGIN: chrono::Duration = chrono::Duration::seconds(60);

/// What an EOD sweep pulled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepReport {
    pub fills: usize,
    pub duplicates: usize,
    pub failed: usize,
    pub fill_gaps: usize,
    /// `(account_ref, scope, error)` for every pull the venue refused.
    pub errors: Vec<(String, String, String)>,
}

pub struct Reconciler {
    connection: String,
    source: Arc<dyn ExecutionSource>,
    engine: EngineHandle,
    pool: PgPool,
    adapter: Option<Arc<dyn BrokerAdapter>>,
    instruments: Option<Arc<dyn InstrumentProvider>>,
    trigger: Notify,
}

/// Stops the reconciler's tasks when the last handle goes.
struct Tasks(std::sync::Mutex<JoinSet<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        if let Ok(set) = self.0.get_mut() {
            set.abort_all();
        }
    }
}

#[derive(Clone)]
pub struct ReconcilerHandle {
    inner: Arc<Reconciler>,
    _tasks: Arc<Tasks>,
}

impl ReconcilerHandle {
    /// Pull now, or once more after the pull already running.
    pub fn pull_now(&self) {
        self.inner.trigger.notify_one();
    }

    /// Pull every fill since `since`, cursors untouched (EOD sweep).
    pub async fn sweep(&self, since: DateTime<Utc>) -> SweepReport {
        self.inner.sweep(since).await
    }
}

/// Start a reconciler without an adapter (no synthetic closes) or instrument provider.
pub fn spawn(
    connection: String,
    source: Arc<dyn ExecutionSource>,
    events: mpsc::Receiver<VenueEvent>,
    engine: EngineHandle,
    pool: PgPool,
) -> ReconcilerHandle {
    spawn_with(connection, source, events, engine, pool, None, None)
}

/// `adapter` answers `order_status` for OMS orders gone from the snapshot;
/// `instruments` resolves symbols ingest does not know.
pub fn spawn_with(
    connection: String,
    source: Arc<dyn ExecutionSource>,
    mut events: mpsc::Receiver<VenueEvent>,
    engine: EngineHandle,
    pool: PgPool,
    adapter: Option<Arc<dyn BrokerAdapter>>,
    instruments: Option<Arc<dyn InstrumentProvider>>,
) -> ReconcilerHandle {
    let inner = Arc::new(Reconciler { connection, source, engine, pool, adapter, instruments, trigger: Notify::new() });
    let mut set = JoinSet::new();

    let r = inner.clone();
    set.spawn(async move {
        let mut gap_rerun = false;
        loop {
            r.trigger.notified().await;
            let gap = r.pull().await;
            // A gap the pull itself found (a snapshot close) gets one more pull
            // for its fills; not an endless loop if the venue never has them.
            if gap && !gap_rerun {
                gap_rerun = true;
                r.trigger.notify_one();
            } else {
                gap_rerun = false;
            }
        }
    });

    let r = inner.clone();
    set.spawn(async move {
        let secs = r.poll_secs().await.max(1);
        let period = Duration::from_secs(secs);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            tick.tick().await;
            r.trigger.notify_one();
        }
    });

    let r = inner.clone();
    set.spawn(async move {
        while let Some(event) = events.recv().await {
            if matches!(event, VenueEvent::Connected) {
                info!(connection = %r.connection, "reconciler: stream connected, pulling");
                r.trigger.notify_one();
                continue;
            }
            // Stream order is kept by the engine's queue; the reply is only
            // watched for a gap, off this loop.
            let (tx, rx) = tokio::sync::oneshot::channel();
            r.engine.venue_send(&r.connection, event, r.instruments.clone(), Some(tx)).await;
            let watcher = r.clone();
            tokio::spawn(async move {
                if let Ok(Ok(IngestOutcome { fill_gap: true, .. })) = rx.await {
                    watcher.trigger.notify_one();
                }
            });
        }
        debug!(connection = %r.connection, "reconciler: stream sink closed");
    });

    ReconcilerHandle { inner, _tasks: Arc::new(Tasks(std::sync::Mutex::new(set))) }
}

impl Reconciler {
    async fn poll_secs(&self) -> u64 {
        let configured: Option<i32> = sqlx::query_scalar("SELECT intraday_poll_secs FROM broker_connection WHERE code = $1")
            .bind(&self.connection)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or_else(|e| {
                warn!(connection = %self.connection, error = %e, "reconciler: poll interval unreadable, using the adapter's");
                None
            })
            .flatten();
        configured.map(|s| s as u64).unwrap_or_else(|| self.source.hints().intraday_secs)
    }

    async fn accounts(&self) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, external_account_ref FROM account \
             WHERE broker_connection_code = $1 AND external_account_ref IS NOT NULL ORDER BY code",
        )
        .bind(&self.connection)
        .fetch_all(&self.pool)
        .await
    }

    async fn ingest(&self, event: VenueEvent) -> Result<IngestOutcome, String> {
        self.engine.venue_ack(&self.connection, event, self.instruments.clone()).await
    }

    /// One intraday pull over every account; returns whether any ingest found a fill gap.
    async fn pull(&self) -> bool {
        let accounts = match self.accounts().await {
            Ok(a) => a,
            Err(e) => {
                error!(connection = %self.connection, error = %e, "reconciler: accounts unreadable, pull skipped");
                return false;
            }
        };
        let gap = Arc::new(AtomicBool::new(false));
        for (account_id, account_ref) in accounts {
            match self.source.scopes(&account_ref).await {
                Ok(scopes) => {
                    for scope in scopes {
                        if let Err(e) = self.pull_scope(account_id, &account_ref, &scope, &gap).await {
                            warn!(connection = %self.connection, account = %account_ref, scope, error = %e, "reconciler: pull stopped");
                        }
                    }
                }
                Err(e) => warn!(connection = %self.connection, account = %account_ref, error = %e, "reconciler: scopes unavailable"),
            }
            if let Err(e) = self.snapshot(account_id, &account_ref, &gap).await {
                warn!(connection = %self.connection, account = %account_ref, error = %e, "reconciler: open-order snapshot failed");
            }
        }
        gap.load(Ordering::Relaxed)
    }

    async fn pull_scope(&self, account_id: Uuid, account_ref: &str, scope: &str, gap: &AtomicBool) -> Result<(), String> {
        let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT cursor, updated_at FROM recon_cursor \
             WHERE broker_connection_code = $1 AND account_id = $2 AND scope = $3",
        )
        .bind(&self.connection)
        .bind(account_id)
        .bind(scope)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some((cursor, since)) = row else {
            // A new account starts now; its history is the EOD sweep's job.
            sqlx::query(
                "INSERT INTO recon_cursor (broker_connection_code, account_id, scope, cursor) \
                 VALUES ($1, $2, $3, '') ON CONFLICT DO NOTHING",
            )
            .bind(&self.connection)
            .bind(account_id)
            .bind(scope)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
            info!(connection = %self.connection, account = account_ref, scope, "reconciler: cursor started at now");
            return Ok(());
        };
        let since = since - SINCE_MARGIN;
        let mut from = (!cursor.is_empty()).then_some(Cursor(cursor));
        loop {
            let page = self.source.fills_since(account_ref, scope, from.as_ref(), since).await.map_err(|e| e.to_string())?;
            for fill in page.fills {
                let exec_id = fill.exec_id.clone();
                let out = self.ingest(VenueEvent::Fill(fill)).await.map_err(|e| format!("fill {exec_id}: {e}"))?;
                if out.fill_gap {
                    gap.store(true, Ordering::Relaxed);
                }
            }
            // An empty `next` keeps the row as is: moving its stamp would slide
            // `since` past fills the venue publishes late.
            sqlx::query(
                "UPDATE recon_cursor SET cursor = $4, updated_at = now() \
                 WHERE broker_connection_code = $1 AND account_id = $2 AND scope = $3 AND $4 <> ''",
            )
            .bind(&self.connection)
            .bind(account_id)
            .bind(scope)
            .bind(&page.next.0)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
            if !page.more {
                return Ok(());
            }
            from = Some(page.next);
        }
    }

    /// Apply the venue's open orders, and close OMS-working orders it no longer lists.
    async fn snapshot(&self, account_id: Uuid, account_ref: &str, gap: &AtomicBool) -> Result<(), String> {
        let open = self.source.open_order_snapshot(account_ref).await.map_err(|e| e.to_string())?;
        for o in &open {
            match self.ingest(VenueEvent::Order(o.clone())).await {
                Ok(out) if out.fill_gap => gap.store(true, Ordering::Relaxed),
                Ok(_) => {}
                Err(e) => warn!(connection = %self.connection, venue_order_id = %o.venue_order_id, error = %e, "reconciler: snapshot order not applied"),
            }
        }
        let working: Vec<(Uuid, Option<String>, String, String, f64, Option<String>)> = sqlx::query_as(
            "SELECT os.order_id, os.external_order_id, os.side, os.order_type, os.original_qty::float8, bi.broker_symbol \
             FROM order_state os \
             JOIN broker_connection bc ON bc.code = os.broker_connection_code \
             LEFT JOIN broker_instrument bi \
               ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = bc.broker_code \
             WHERE os.broker_connection_code = $1 AND os.account_id = $2 \
               AND os.status IN ('routed', 'partially_filled')",
        )
        .bind(&self.connection)
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        for (order_id, ext_id, side, order_type, qty, symbol) in working {
            let id = order_id.to_string();
            let listed = open.iter().any(|o| {
                o.client_order_id.as_deref() == Some(id.as_str()) || ext_id.as_deref() == Some(o.venue_order_id.as_str())
            });
            if listed {
                continue;
            }
            let Some(adapter) = &self.adapter else {
                warn!(order_id = %order_id, "reconciler: working order gone from the venue, no adapter to resolve it");
                continue;
            };
            let ext = ext_id.unwrap_or_default();
            let symbol = symbol.unwrap_or_default();
            let st = match adapter.order_status(&id, &ext, &symbol).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(order_id = %order_id, error = %e, "reconciler: order_status failed");
                    continue;
                }
            };
            let status = match st.outcome {
                BrokerOrderOutcome::Filled => VenueOrderStatus::Filled,
                BrokerOrderOutcome::Canceled => VenueOrderStatus::Canceled,
                BrokerOrderOutcome::Rejected => VenueOrderStatus::Rejected { reason: "resolved by reconciliation".into() },
                BrokerOrderOutcome::Working => {
                    warn!(order_id = %order_id, "reconciler: venue says working but omits it from open orders");
                    continue;
                }
            };
            let update = VenueOrderUpdate {
                venue_order_id: ext,
                client_order_id: Some(id),
                account_ref: account_ref.to_string(),
                symbol,
                buy: side.eq_ignore_ascii_case("buy"),
                order_type,
                qty,
                limit_px: None,
                status,
                cum_qty: st.executed_qty,
                ts: None,
            };
            match self.ingest(VenueEvent::Order(update)).await {
                Ok(out) if out.fill_gap => gap.store(true, Ordering::Relaxed),
                Ok(_) => info!(order_id = %order_id, "reconciler: resolved an order gone from the venue"),
                Err(e) => warn!(order_id = %order_id, error = %e, "reconciler: close not applied"),
            }
        }
        Ok(())
    }

    async fn sweep(&self, since: DateTime<Utc>) -> SweepReport {
        let mut report = SweepReport::default();
        let accounts = match self.accounts().await {
            Ok(a) => a,
            Err(e) => {
                report.errors.push((String::new(), String::new(), e.to_string()));
                return report;
            }
        };
        for (_, account_ref) in accounts {
            let scopes = match self.source.scopes(&account_ref).await {
                Ok(s) => s,
                Err(e) => {
                    report.errors.push((account_ref.clone(), String::new(), e.to_string()));
                    continue;
                }
            };
            for scope in scopes {
                let mut from: Option<Cursor> = None;
                loop {
                    let page = match self.source.fills_since(&account_ref, &scope, from.as_ref(), since).await {
                        Ok(p) => p,
                        Err(e) => {
                            report.errors.push((account_ref.clone(), scope.clone(), e.to_string()));
                            break;
                        }
                    };
                    for fill in page.fills {
                        report.fills += 1;
                        match self.ingest(VenueEvent::Fill(fill)).await {
                            Ok(out) => {
                                report.duplicates += out.duplicate as usize;
                                report.fill_gaps += out.fill_gap as usize;
                            }
                            Err(_) => report.failed += 1,
                        }
                    }
                    if !page.more {
                        break;
                    }
                    from = Some(page.next);
                }
            }
        }
        report
    }
}

#[cfg(test)]
#[path = "reconciler_tests.rs"]
mod tests;
