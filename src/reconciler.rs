//! One reconciler per venue connection: the single place that moves its venue
//! events into the engine and fills the gaps the stream leaves.
//!
//! - **Stream:** events from the connection's [`VenueStream`] sink are forwarded
//!   to the engine in order. `Connected` triggers a pull instead.
//! - **Intraday pull:** on `Connected`, on a timer (`broker_connection.intraday_poll_secs`,
//!   else `hints().intraday_secs`) and, in tests, on `ReconcilerHandle::pull_now`. For each
//!   account of the connection × `scopes()`, fills are paged from the stored
//!   `recon_cursor`. Each fill is ingested and **awaited**; the cursor advances only
//!   once a whole page is booked, so a failure re-pulls that page next time
//!   (ingest dedupes on exec id). Then the open-order snapshot is applied, and an
//!   OMS-working order missing from it is closed with the status the venue gives
//!   for it (the port of `recon_orders`' `ResolveTerminal`). That close books no
//!   fills: ingest flags a fill gap and the gap triggers another pull.
//! - **Triggers coalesce:** one pull runs at a time; any number of triggers while
//!   it runs queue exactly one rerun.
//! - **Cursors:** a missing cursor on an account that has none yet is created
//!   empty, stamped "now", and history is skipped (history comes only from the
//!   EOD sweep). A missing cursor on an account that already has cursors (a pair
//!   found on reconnect, or newly traded) is stamped with the account's oldest
//!   cursor stamp and pulled from there at once.
//! - **Watermark:** fills executed before `broker_connection.recon_not_before`
//!   are dropped by pulls and sweeps (pre-upgrade fills the ledger could not re-key). An empty cursor pulls
//!   with `from = None, since = updated_at - 60s`; a non-empty returned cursor is
//!   stored, an empty one leaves the row (and its stamp) untouched.
//!
//! **Accounts:** a connection's reconciler covers its accounts except those a
//! co-located strategy's follow covers ([`Accounts::Unfollowed`]). A follow
//! is a reconciler of its own on the same connection code, scoped to the one
//! account ([`Accounts::Only`]), pulling with the strategy's follow key: the
//! connection's key cannot see a sub-account's trades, and pulling with it
//! for the sub-account would book the main account's trades there. An account
//! counts as followed while its COLOCATED strategy has a follow credential,
//! even when that follow failed to start (it is then pulled by nobody).
//!
//! **Account ref:** a stream event without one (Binance's user-data stream,
//! which does not know it) gets the ref of the reconciler's only account;
//! with several, it is left empty and ingest refuses it (unknown account).
//!
//! **FIX:** the session's QuickFIX callbacks emit the venue events; its
//! `VenueStream` drains them into this reconciler's sink like any other
//! stream, and `Connected` is sent on every (re)logon. Binance FIX pulls from
//! the same credential's REST API; IBKR FIX's source is the FIX adapter itself
//! (empty pulls, 35=AF open-order snapshot).
//!
//! The instrument provider (from `Connector::universe_provider`) rides along on
//! every `GatewayMsg::Venue` this sends, so the engine keeps no per-connection map.


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
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SweepReport {
    /// What the lookback asked for.
    pub requested_from: DateTime<Utc>,
    /// What the venue serves: `requested_from`, or later when the venue's
    /// history is shorter.
    pub obtained_from: DateTime<Utc>,
    pub booked: u32,
    pub duplicates: u32,
    /// Fills recorded as breaks (not booked).
    pub breaks: u32,
    /// Fills whose ingest errored.
    pub failed: u32,
    /// Accounts or scopes the venue refused, as text.
    pub errors: Vec<String>,
}

impl SweepReport {
    /// Two sweeps of one connection (its own and a follow's) as one.
    pub fn merge(mut self, other: SweepReport) -> SweepReport {
        self.obtained_from = self.obtained_from.max(other.obtained_from);
        self.booked += other.booked;
        self.duplicates += other.duplicates;
        self.breaks += other.breaks;
        self.failed += other.failed;
        self.errors.extend(other.errors);
        self
    }
}

/// Which of the connection's accounts a reconciler covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Accounts {
    /// Every account of the connection that no co-located follow covers.
    Unfollowed,
    /// One account, followed with its own key.
    Only(Uuid),
}

impl Accounts {
    /// The reconciler's name in the live registry: the connection code, or
    /// `"<connection>#<account id>"` for a follow.
    pub fn key(&self, connection: &str) -> String {
        match self {
            Self::Unfollowed => connection.to_string(),
            Self::Only(id) => format!("{connection}#{id}"),
        }
    }
}

pub struct Reconciler {
    connection: String,
    scope: Accounts,
    source: Arc<dyn ExecutionSource>,
    engine: EngineHandle,
    pool: PgPool,
    adapter: Option<Arc<dyn BrokerAdapter>>,
    instruments: Option<Arc<dyn InstrumentProvider>>,
    trigger: Notify,
    /// The next pull also discovers pairs from the account's balances (set on `Connected`).
    discover: AtomicBool,
    /// Consecutive ingest failures of one exec id, per (account, scope).
    failures: std::sync::Mutex<std::collections::HashMap<(Uuid, String), (String, u32)>>,
}

/// After this many consecutive failures of one fill, the fill is recorded as an
/// `ingest_error` break and the scope moves past it.
const MAX_FILL_FAILURES: u32 = 3;

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
    #[cfg(test)]
    pub fn pull_now(&self) {
        self.inner.trigger.notify_one();
    }

    /// EOD sweep: pull every fill from `now - lookback` (clamped to the venue's
    /// history), cursors untouched.
    pub async fn sweep(&self, lookback: chrono::Duration) -> SweepReport {
        self.inner.sweep(Utc::now(), lookback).await
    }

    #[cfg(test)]
    pub(crate) async fn sweep_at(&self, now: DateTime<Utc>, lookback: chrono::Duration) -> SweepReport {
        self.inner.sweep(now, lookback).await
    }

    fn downgrade(&self) -> WeakHandle {
        WeakHandle { inner: Arc::downgrade(&self.inner), tasks: Arc::downgrade(&self._tasks) }
    }
}

/// A handle that does not keep the reconciler alive.
struct WeakHandle {
    inner: std::sync::Weak<Reconciler>,
    tasks: std::sync::Weak<Tasks>,
}

/// Reconcilers by connection, so a scheduled job can reach the current one
/// without holding a handle of its own: a held handle would keep the tasks
/// running past a stream restart. Entries are weak, replaced when a connection
/// respawns, and dead once the stream (the only owner) is gone.
static LIVE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, WeakHandle>>> =
    std::sync::LazyLock::new(Default::default);

/// The running reconciler under `key` ([`Accounts::key`]), if any. Hold the result only briefly.
pub fn live(key: &str) -> Option<ReconcilerHandle> {
    let mut live = LIVE.lock().unwrap();
    let up = live.get(key).and_then(|w| Some(ReconcilerHandle { inner: w.inner.upgrade()?, _tasks: w.tasks.upgrade()? }));
    if up.is_none() {
        live.remove(key);
    }
    up
}

/// Every running reconciler of `connection`: its own and its follows'.
pub fn live_all(connection: &str) -> Vec<ReconcilerHandle> {
    let follow = format!("{connection}#");
    let keys: Vec<String> =
        LIVE.lock().unwrap().keys().filter(|k| *k == connection || k.starts_with(&follow)).cloned().collect();
    keys.iter().filter_map(|k| live(k)).collect()
}

/// Start a reconciler without an adapter (no synthetic closes) or instrument provider.
#[cfg(test)]
pub fn spawn(
    connection: String,
    source: Arc<dyn ExecutionSource>,
    events: mpsc::Receiver<VenueEvent>,
    engine: EngineHandle,
    pool: PgPool,
) -> ReconcilerHandle {
    spawn_with(connection, source, events, engine, pool, None, None, Accounts::Unfollowed)
}

/// `adapter` answers `order_status` for OMS orders gone from the snapshot;
/// `instruments` resolves symbols ingest does not know; `scope` picks the accounts.
#[allow(clippy::too_many_arguments)]
pub fn spawn_with(
    connection: String,
    source: Arc<dyn ExecutionSource>,
    mut events: mpsc::Receiver<VenueEvent>,
    engine: EngineHandle,
    pool: PgPool,
    adapter: Option<Arc<dyn BrokerAdapter>>,
    instruments: Option<Arc<dyn InstrumentProvider>>,
    scope: Accounts,
) -> ReconcilerHandle {
    let name = scope.key(&connection);
    let inner = Arc::new(Reconciler { connection, scope, source, engine, pool, adapter, instruments, trigger: Notify::new(), discover: AtomicBool::new(false), failures: Default::default() });
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
        while let Some(mut event) = events.recv().await {
            if matches!(event, VenueEvent::Connected) {
                info!(connection = %r.connection, "reconciler: stream connected, pulling");
                r.discover.store(true, Ordering::Relaxed);
                r.trigger.notify_one();
                continue;
            }
            r.stamp_account(&mut event).await;
            // Stream order is kept by the engine's queue; the reply is only
            // watched for a gap, off this loop.
            let (tx, rx) = tokio::sync::oneshot::channel();
            r.engine.venue_send(&r.connection, event, r.instruments.clone(), Some(tx)).await;
            let watcher = r.clone();
            tokio::spawn(async move {
                match rx.await {
                    Ok(Ok(IngestOutcome { fill_gap: true, .. })) => watcher.trigger.notify_one(),
                    Ok(Err(e)) => warn!(connection = %watcher.connection, error = %e, "reconciler: stream event not ingested; the next pull retries it"),
                    _ => {}
                }
            });
        }
        debug!(connection = %r.connection, "reconciler: stream sink closed");
    });

    let handle = ReconcilerHandle { inner, _tasks: Arc::new(Tasks(std::sync::Mutex::new(set))) };
    LIVE.lock().unwrap().insert(name, handle.downgrade());
    handle
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

    /// The accounts this reconciler covers (see [`Accounts`]). One row per ref:
    /// the oldest, as in ingest's `resolve_account`; only a followed oldest row
    /// (excluded before the pick) could make the two differ.
    async fn accounts(&self) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
        match self.scope {
            Accounts::Unfollowed => {
                sqlx::query_as(
                    "SELECT id, external_account_ref FROM ( \
                     SELECT DISTINCT ON (a.external_account_ref) a.id, a.external_account_ref, a.code FROM account a \
                     WHERE a.broker_connection_code = $1 AND a.external_account_ref IS NOT NULL \
                       AND NOT EXISTS ( \
                         SELECT 1 FROM strategy_connection sc JOIN strategy s ON s.code = sc.strategy_code \
                         WHERE sc.account_id = a.id AND s.mode = 'COLOCATED' \
                           AND s.follow_credentials IS NOT NULL AND s.follow_environment IS NOT NULL) \
                     ORDER BY a.external_account_ref, a.created_at, a.id) r \
                     ORDER BY code",
                )
                .bind(&self.connection)
                .fetch_all(&self.pool)
                .await
            }
            Accounts::Only(id) => {
                sqlx::query_as(
                    "SELECT id, external_account_ref FROM account \
                     WHERE broker_connection_code = $1 AND id = $2 AND external_account_ref IS NOT NULL",
                )
                .bind(&self.connection)
                .bind(id)
                .fetch_all(&self.pool)
                .await
            }
        }
    }

    /// Give a stream event without an account ref the ref of this
    /// reconciler's only account; with none or several, leave it empty.
    async fn stamp_account(&self, event: &mut VenueEvent) {
        let slot = match event {
            VenueEvent::Fill(f) => &mut f.account_ref,
            VenueEvent::Order(o) => &mut o.account_ref,
            VenueEvent::Fee { account_ref, .. } => account_ref,
            VenueEvent::Connected | VenueEvent::Disconnected => return,
        };
        if !slot.is_empty() {
            return;
        }
        match self.accounts().await {
            Ok(accounts) => match accounts.as_slice() {
                [(_, account_ref)] => *slot = account_ref.clone(),
                [] => {}
                several => warn!(
                    connection = %self.connection,
                    accounts = several.len(),
                    "reconciler: stream event without an account ref and several accounts to pick from; ingest will refuse it"
                ),
            },
            Err(e) => warn!(connection = %self.connection, error = %e, "reconciler: accounts unreadable, event left without an account"),
        }
    }

    /// The scopes to pull for one account: the venue's own, plus — when the
    /// venue pulls per symbol — every symbol the OMS has orders on for the
    /// account (the venue only knows its open orders' symbols; a filled or
    /// cancelled order's symbol would otherwise never be pulled). With
    /// `discover` (on reconnect and in the EOD sweep), also every catalog pair
    /// whose base asset the account holds, so a pair traded entirely while the
    /// stream was down is found. Not found: a never-seen pair bought and sold
    /// back to zero during an outage (no balance, no order, no open order).
    async fn scopes(&self, account_id: Uuid, account_ref: &str, discover: bool) -> Result<Vec<String>, String> {
        let mut scopes: std::collections::BTreeSet<String> =
            self.source.scopes(account_ref).await.map_err(|e| e.to_string())?.into_iter().collect();
        if self.source.hints().per_symbol {
            let booked: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT bi.broker_symbol FROM order_state os \
                 JOIN broker_connection bc ON bc.code = $1 \
                 JOIN broker_instrument bi ON bi.instrument_id::text = os.instrument_id AND bi.broker_code = bc.broker_code \
                 WHERE os.account_id = $2",
            )
            .bind(&self.connection)
            .bind(account_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
            scopes.extend(booked);
            if discover {
                // A failed balance read costs only the discovery, retried on
                // the next pull; the known scopes are still pulled.
                let held = match self.source.held_assets(account_ref).await {
                    Ok(held) => held,
                    Err(e) => {
                        warn!(connection = %self.connection, account = account_ref, error = %e, "reconciler: balances unavailable; pair discovery retried next pull");
                        self.discover.store(true, Ordering::Relaxed);
                        Vec::new()
                    }
                };
                if !held.is_empty() {
                    let found: Vec<String> = sqlx::query_scalar(
                        "SELECT DISTINCT bi.broker_symbol FROM broker_connection bc \
                         JOIN broker_instrument bi ON bi.broker_code = bc.broker_code \
                         JOIN instrument i ON i.id = bi.instrument_id \
                         WHERE bc.code = $1 AND length(bi.broker_symbol) > length(i.currency) \
                           AND right(bi.broker_symbol, length(i.currency)) = i.currency \
                           AND left(bi.broker_symbol, length(bi.broker_symbol) - length(i.currency)) = ANY($2)",
                    )
                    .bind(&self.connection)
                    .bind(&held)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| e.to_string())?;
                    scopes.extend(found);
                }
            }
        }
        Ok(scopes.into_iter().collect())
    }

    /// `broker_connection.recon_not_before`: fills executed before it are not
    /// pulled (pre-upgrade fills whose ids the ledger could not re-key).
    async fn not_before(&self) -> Result<Option<DateTime<Utc>>, String> {
        sqlx::query_scalar::<_, Option<DateTime<Utc>>>("SELECT recon_not_before FROM broker_connection WHERE code = $1")
            .bind(&self.connection)
            .fetch_optional(&self.pool)
            .await
            .map(Option::flatten)
            .map_err(|e| e.to_string())
    }

    /// Count one failure of `exec_id` in a scope; `true` once it has failed
    /// [`MAX_FILL_FAILURES`] times in a row (the count is then reset).
    fn note_failure(&self, account_id: Uuid, scope: &str, exec_id: &str) -> bool {
        let mut map = self.failures.lock().unwrap();
        let entry = map.entry((account_id, scope.to_string())).or_insert_with(|| (exec_id.to_string(), 0));
        if entry.0 != exec_id {
            *entry = (exec_id.to_string(), 0);
        }
        entry.1 += 1;
        if entry.1 >= MAX_FILL_FAILURES {
            map.remove(&(account_id, scope.to_string()));
            true
        } else {
            false
        }
    }

    fn clear_failures(&self, account_id: Uuid, scope: &str) {
        self.failures.lock().unwrap().remove(&(account_id, scope.to_string()));
    }

    /// Record a fill that keeps failing to ingest as an `ingest_error` break, so
    /// a person sees it and the scope is not wedged on it.
    async fn record_ingest_error(&self, account_id: Uuid, fill: &gateway::venue_event::VenueFill, error: &str) -> Result<(), String> {
        let mut event = crate::ingest::apply::event_json(&VenueEvent::Fill(fill.clone()));
        event["error"] = serde_json::Value::String(error.to_string());
        sqlx::query(
            "INSERT INTO recon_break (id, broker_connection_code, account_id, venue_order_id, kind, events_json) \
             VALUES ($1, $2, $3, $4, 'ingest_error', jsonb_build_array($5::jsonb)) \
             ON CONFLICT (broker_connection_code, account_id, venue_order_id) WHERE status = 'open' \
             DO UPDATE SET events_json = recon_break.events_json || EXCLUDED.events_json",
        )
        .bind(Uuid::new_v4())
        .bind(&self.connection)
        .bind(account_id)
        .bind(&fill.venue_order_id)
        .bind(event)
        .execute(&self.pool)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
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
        let discover = self.discover.swap(false, Ordering::Relaxed);
        for (account_id, account_ref) in accounts {
            match self.scopes(account_id, &account_ref, discover).await {
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
        let (cursor, since) = match row {
            Some(row) => row,
            None => {
                // A new scope on an account already pulled (a pair found by
                // balance discovery, or newly traded) starts where the account's
                // oldest cursor stands, so fills from an outage are pulled now;
                // a brand-new account starts now (its history is the EOD sweep's).
                let watermark: Option<DateTime<Utc>> = sqlx::query_scalar(
                    "SELECT min(updated_at) FROM recon_cursor WHERE broker_connection_code = $1 AND account_id = $2",
                )
                .bind(&self.connection)
                .bind(account_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
                sqlx::query(
                    "INSERT INTO recon_cursor (broker_connection_code, account_id, scope, cursor, updated_at) \
                     VALUES ($1, $2, $3, '', COALESCE($4, now())) ON CONFLICT DO NOTHING",
                )
                .bind(&self.connection)
                .bind(account_id)
                .bind(scope)
                .bind(watermark)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
                match watermark {
                    Some(at) => {
                        info!(connection = %self.connection, account = account_ref, scope, since = %at, "reconciler: new scope starts at the account's watermark");
                        (String::new(), at)
                    }
                    None => {
                        info!(connection = %self.connection, account = account_ref, scope, "reconciler: cursor started at now");
                        return Ok(());
                    }
                }
            }
        };
        let not_before = self.not_before().await?;
        // A scan from no cursor keeps one anchored `since` on every page; one
        // resuming from a cursor filters nothing (all after it is new).
        let since = if cursor.is_empty() { since - SINCE_MARGIN } else { DateTime::<Utc>::MIN_UTC };
        let mut from = (!cursor.is_empty()).then_some(Cursor(cursor));
        loop {
            let page = self.source.fills_since(account_ref, scope, from.as_ref(), since).await.map_err(|e| e.to_string())?;
            for fill in page.fills {
                if before(&fill, not_before) {
                    continue;
                }
                let exec_id = fill.exec_id.clone();
                let out = match self.ingest(VenueEvent::Fill(fill.clone())).await {
                    Ok(out) => out,
                    Err(e) => {
                        warn!(connection = %self.connection, account = account_ref, scope, exec_id, error = %e, "reconciler: fill not ingested");
                        if !self.note_failure(account_id, scope, &exec_id) {
                            return Err(format!("fill {exec_id}: {e}"));
                        }
                        // Persistently failing: a break for a person, and move on.
                        self.record_ingest_error(account_id, &fill, &e).await?;
                        error!(connection = %self.connection, account = account_ref, scope, exec_id, error = %e, "reconciler: fill failed {MAX_FILL_FAILURES} times; recorded as an ingest_error break and skipped");
                        continue;
                    }
                };
                if out.fill_gap {
                    gap.store(true, Ordering::Relaxed);
                }
            }
            self.clear_failures(account_id, scope);
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
            if page.next.0.is_empty() {
                warn!(connection = %self.connection, account = account_ref, scope, "reconciler: venue says more but gave no cursor; stopping this scope");
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
                created_at: None,
            };
            match self.ingest(VenueEvent::Order(update)).await {
                Ok(out) if out.fill_gap => gap.store(true, Ordering::Relaxed),
                Ok(_) => info!(order_id = %order_id, "reconciler: resolved an order gone from the venue"),
                Err(e) => warn!(order_id = %order_id, error = %e, "reconciler: close not applied"),
            }
        }
        Ok(())
    }

    async fn sweep(&self, now: DateTime<Utc>, lookback: chrono::Duration) -> SweepReport {
        let requested_from = now - lookback;
        let obtained_from = match self.source.hints().max_history {
            Some(max) => requested_from.max(now - max),
            None => requested_from,
        };
        let mut report = SweepReport { requested_from, obtained_from, booked: 0, duplicates: 0, breaks: 0, failed: 0, errors: vec![] };
        let accounts = match self.accounts().await {
            Ok(a) => a,
            Err(e) => {
                report.errors.push(format!("accounts: {e}"));
                return report;
            }
        };
        let not_before = match self.not_before().await {
            Ok(n) => n,
            Err(e) => {
                report.errors.push(format!("recon_not_before: {e}"));
                return report;
            }
        };
        for (account_id, account_ref) in accounts {
            let scopes = match self.scopes(account_id, &account_ref, true).await {
                Ok(s) => s,
                Err(e) => {
                    report.errors.push(format!("{account_ref}: scopes: {e}"));
                    continue;
                }
            };
            for scope in scopes {
                let mut from: Option<Cursor> = None;
                loop {
                    let page = match self.source.fills_since(&account_ref, &scope, from.as_ref(), obtained_from).await {
                        Ok(p) => p,
                        Err(e) => {
                            report.errors.push(format!("{account_ref} {scope}: {e}"));
                            break;
                        }
                    };
                    for fill in page.fills {
                        if before(&fill, not_before) {
                            continue;
                        }
                        let exec_id = fill.exec_id.clone();
                        match self.ingest(VenueEvent::Fill(fill)).await {
                            Ok(out) if out.duplicate => report.duplicates += 1,
                            Ok(out) if out.broken => report.breaks += 1,
                            Ok(_) => report.booked += 1,
                            Err(e) => {
                                warn!(connection = %self.connection, account = %account_ref, scope, exec_id, error = %e, "reconciler: sweep: fill not ingested");
                                report.failed += 1;
                                report.errors.push(format!("{account_ref} {scope}: fill {exec_id}: {e}"));
                            }
                        }
                    }
                    if !page.more {
                        break;
                    }
                    if page.next.0.is_empty() {
                        warn!(connection = %self.connection, account = %account_ref, scope, "reconciler: sweep: venue says more but gave no cursor; stopping this scope");
                        break;
                    }
                    from = Some(page.next);
                }
            }
        }
        report
    }
}

/// A fill executed before the connection's `recon_not_before` (one with no
/// execution time is kept).
fn before(fill: &gateway::venue_event::VenueFill, not_before: Option<DateTime<Utc>>) -> bool {
    matches!((fill.executed_at, not_before), (Some(at), Some(nb)) if at < nb)
}

#[cfg(test)]
#[path = "reconciler_tests.rs"]
mod tests;
