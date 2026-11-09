//! Embedded runtime orchestration: recovery, serialized admission, durable
//! journal write before send, and a runner that never holds the risk lock
//! across a venue call.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use colo_link::v2::{
    Bootstrap, ControlAckStage, FromRuntime, Received, RuntimeCheckpoint, RuntimeLink, ToRuntime,
    PROTOCOL_VERSION,
};
use gateway::adapter::BrokerError;
use gateway::runtime::{
    AccountIdentity, RuntimeEvent, RuntimeOrder, RuntimeOrderState, RuntimeVenue, StreamState,
    SubmitOutcome, VenueEvent,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::book::{Book, InstrumentPrices, Intent, Prices, Submission};
use crate::control::{ControlAction, ControlState, RuntimeState};
use crate::journal::{Journal, JournalRecord};
use crate::recovery::{recover, RecoveredBook};
use crate::reporter::Reporter;
use crate::RuntimeError;

pub struct RuntimeConfig {
    pub identity: AccountIdentity,
    pub epoch: u64,
    pub bootstrap: Bootstrap,
    pub journal_path: PathBuf,
    pub max_journal_bytes: u64,
    pub event_queue_capacity: usize,
    pub control_url: Option<String>,
    pub events_url: Option<String>,
    pub credential_reference: Option<String>,
    /// Injected by the caller; the runtime never sees the token or CA material.
    pub link: Option<Arc<dyn RuntimeLink>>,
    /// Stable for this process/run; reconnects reuse it. A new run is a new
    /// owner and requires the OMS's verified-stop takeover rule.
    pub session_nonce: String,
}

struct Core {
    book: Book,
    control: ControlState,
    prices: Prices,
    closed: bool,
    /// The kill command still waiting for the venue to show no working orders.
    pending_clear: Option<String>,
}

struct Inner {
    core: AsyncMutex<Core>,
    journal: Arc<Mutex<Journal>>,
    venue: Arc<dyn RuntimeVenue>,
    events: Mutex<VecDeque<RuntimeEvent>>,
    capacity: usize,
    journal_limit: u64,
    identity: AccountIdentity,
    epoch: u64,
    /// Messages for the OMS that are not journal events (hello, recovery
    /// status, acknowledgements). `None` without a link.
    outbox: Option<tokio::sync::mpsc::UnboundedSender<FromRuntime>>,
    suspensions: Mutex<std::collections::BTreeSet<Suspension>>,
    /// Book-changing journal writes not yet applied to the book. A checkpoint
    /// is only taken at zero, so it never claims a record the book lacks.
    writing: AtomicUsize,
    /// A resync after feed loss is running.
    resyncing: AtomicBool,
    /// Set once and never cleared, so a task that checks late still stops.
    shutdown: tokio::sync::watch::Sender<bool>,
    report_now: tokio::sync::Notify,
    recovered: AtomicBool,
}

pub struct Runtime {
    inner: Arc<Inner>,
    events_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    link_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Runtime {
    pub async fn start(config: RuntimeConfig, venue: Arc<dyn RuntimeVenue>) -> Result<Self, RuntimeError> {
        if config.max_journal_bytes == 0 || config.event_queue_capacity == 0 {
            return Err(RuntimeError::InvalidConfig("queue and journal limits must be explicit".into()));
        }
        let mut journal = Journal::open(&config.journal_path, &config.identity)?;
        if journal.byte_len() > config.max_journal_bytes {
            return Err(RuntimeError::InvalidConfig("journal is over its configured limit".into()));
        }
        let journal_killed = journal.kill_latched();
        let recovered = recover(&*venue, &mut journal, &config.bootstrap).await?;
        let RecoveredBook { mut book, events, complete: recovered_ok, evidence, execution_cursor, .. } = recovered;
        book.restore_sequence(journal.last_sequence());
        book.set_journal_id(journal.id());
        book.install_policy(
            config.bootstrap.limits.clone(),
            config.bootstrap.trading_state,
            config.bootstrap.instrument_states.clone(),
        );
        book.set_allocation(
            config.bootstrap.allocation.quote_asset.clone(),
            config.bootstrap.allocation.max_notional,
            config.bootstrap.allocation.max_position_qty,
        );
        book.set_fee_allowance_bps(config.bootstrap.allocation.fee_allowance_bps)?;
        book.set_max_price_age(std::time::Duration::from_secs(
            config.bootstrap.timings.market_data_max_age_secs.max(1),
        ));
        let mut control = ControlState::new(config.epoch);
        let now = Instant::now();
        control.apply(Received::new(
            ToRuntime::Bootstrap { bootstrap: config.bootstrap.clone() },
            now,
        ))?;
        control.begin_recovery();
        if journal_killed {
            control.apply(Received::new(
                ToRuntime::Kill { kill: colo_link::v2::Kill { command_id: "journal-latch".into() } },
                now,
            ))?;
        }
        if control.kill_latched().is_some() {
            book.set_trading_state_halted();
        }
        if recovered_ok && recovered_complete(&book, &control, config.bootstrap.killed_epoch, journal_killed) {
            control.mark_ready();
        } else {
            control.suspend("recovery is incomplete");
        }
        let reporter = if config.link.is_some() {
            Some(Arc::new(AsyncMutex::new(Reporter::new(
                config.identity.clone(),
                config.epoch,
                config.session_nonce.clone(),
                config.bootstrap.baseline.sequence,
                &journal,
            )?)))
        } else {
            None
        };
        let (outbox, outbox_rx) = if config.link.is_some() {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            // The OMS hears who it is talking to and how recovery went before
            // any event or acknowledgement.
            let _ = tx.send(FromRuntime::Hello { versions: vec![PROTOCOL_VERSION], last_acked_control: 0 });
            let _ = tx.send(FromRuntime::RecoveryStatus {
                ready: control.state() == RuntimeState::Ready,
                checkpoint: RuntimeCheckpoint {
                    sequence: journal.last_sequence(),
                    positions: book.checkpoint().positions,
                    open_client_ids: book.all_working_client_ids(),
                    execution_cursor,
                    complete: recovered_ok,
                },
                missing: evidence,
            });
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let inner = Arc::new(Inner {
            core: AsyncMutex::new(Core {
                book,
                control,
                prices: Prices::new(),
                closed: false,
                pending_clear: None,
            }),
            journal: Arc::new(Mutex::new(journal)),
            venue,
            events: Mutex::new(VecDeque::new()),
            capacity: config.event_queue_capacity,
            journal_limit: config.max_journal_bytes,
            identity: config.identity,
            epoch: config.epoch,
            outbox,
            suspensions: Mutex::new(std::collections::BTreeSet::new()),
            writing: AtomicUsize::new(0),
            resyncing: AtomicBool::new(false),
            shutdown: tokio::sync::watch::channel(false).0,
            report_now: tokio::sync::Notify::new(),
            recovered: AtomicBool::new(recovered_ok),
        });
        let task = {
            let weak = Arc::downgrade(&inner);
            spawn_venue_events(weak, events)
        };
        let mut link_tasks = match (&config.link, reporter) {
            (Some(link), Some(reporter)) => spawn_link_tasks(
                Arc::downgrade(&inner),
                link.clone(),
                reporter,
                outbox_rx.expect("a link has an outbox"),
            ),
            _ => Vec::new(),
        };
        inner.raise_drift(&*inner.core.lock().await);
        link_tasks.push(spawn_compaction(Arc::downgrade(&inner)));
        link_tasks.push(spawn_balance_poll(Arc::downgrade(&inner)));
        Ok(Self {
            inner,
            events_task: Mutex::new(Some(task)),
            link_tasks: Mutex::new(link_tasks),
        })
    }

    pub fn identity(&self) -> &AccountIdentity {
        &self.inner.identity
    }

    pub async fn state(&self) -> RuntimeState {
        self.inner.core.lock().await.control.state()
    }

    pub async fn is_ready(&self) -> bool {
        self.state().await == RuntimeState::Ready
    }

    pub async fn set_price(&self, instrument: impl Into<String>, prices: InstrumentPrices) {
        self.inner.core.lock().await.prices.set(instrument, prices);
    }

    /// Public for tests and the control reader: applies a receipt-time message
    /// without touching venue I/O.
    pub async fn apply_control(&self, message: Received<ToRuntime>) -> Result<Vec<ControlAction>, RuntimeError> {
        apply_control_inner(&self.inner, message).await
    }

    pub async fn submit(&self, intent: Intent) -> Result<Submission, RuntimeError> {
        let now = Instant::now();
        let prepared = {
            let mut core = self.inner.core.lock().await;
            if core.closed {
                return Err(RuntimeError::NotReady("runtime is shut down".into()));
            }
            if let Some(reasons) = self.inner.suspension_reasons() {
                return Err(RuntimeError::NotReady(format!("runtime is suspended: {reasons}")));
            }
            core.control.may_submit(now)?;
            let prices = core.prices.clone();
            let writing = self.inner.writing();
            (core.book.prepare(intent, &prices, now)?, writing)
        };
        let (prepared, writing) = prepared;
        let bytes = self.inner.journal.lock().map_err(|_| RuntimeError::JournalIo("journal lock".into()))?.byte_len();
        if bytes > self.inner.journal_limit {
            self.inner.suspend(Suspension::Journal);
            self.inner.core.lock().await.book.abort_order(&prepared.submission.client_order_id);
            return Err(RuntimeError::NotReady("journal is over its limit".into()));
        }
        let record = JournalRecord::Intent {
            submission: prepared.submission.clone(),
            intent: prepared.intent.clone(),
            reservation: prepared.reservation.clone(),
        };
        if let Err(e) = append(&self.inner.journal, record).await {
            self.inner.suspend(Suspension::Journal);
            self.inner.core.lock().await.book.abort_order(&prepared.submission.client_order_id);
            return Err(e);
        }
        drop(writing);
        self.inner.report_now.notify_one();

        let lapsed = self.inner.core.lock().await.control.may_submit(Instant::now()).err();
        if let Some(e) = lapsed {
            abandon(&self.inner, &prepared.submission.client_order_id, "admission closed before send").await;
            return Err(e);
        }

        let request = prepared.broker_request.clone();
        let submission = prepared.submission.clone();
        let task_submission = submission.clone();
        let intent = prepared.intent.clone();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let not_sent = {
                let core = inner.core.lock().await;
                if core.book.cancel_requested(&task_submission.client_order_id) {
                    Some("cancelled before send")
                } else if core.control.may_submit(Instant::now()).is_err() {
                    Some("admission stopped before send")
                } else {
                    None
                }
            };
            if let Some(reason) = not_sent {
                abandon(&inner, &task_submission.client_order_id, reason).await;
                return;
            }
            let result = inner.venue.submit_order(&request).await;
            let (state, venue_id, outcome) = match result {
                Ok(response) => {
                    let venue_order_id = response.external_order_id.clone();
                    (RuntimeOrderState::New, response.external_order_id, SubmitOutcome::Accepted { venue_order_id })
                }
                Err(BrokerError::BrokerRejected(reason)) => (
                    RuntimeOrderState::Rejected,
                    String::new(),
                    SubmitOutcome::Rejected { reason },
                ),
                Err(e) => (
                    RuntimeOrderState::Unknown,
                    String::new(),
                    SubmitOutcome::Unknown { reason: e.to_string() },
                ),
            };
            if state == RuntimeOrderState::Unknown {
                inner.suspend(Suspension::UnclearOrder(task_submission.client_order_id.clone()));
            }
            let order = RuntimeOrder {
                client_order_id: task_submission.client_order_id.clone(),
                venue_order_id: venue_id,
                symbol: intent.instrument.clone(),
                side: intent.side,
                original_quantity: intent.quantity,
                executed_quantity: 0.0,
                leaves_quantity: if state == RuntimeOrderState::Rejected { 0.0 } else { intent.quantity },
                limit_price: intent.limit_price,
                state,
                original_time: Utc::now(),
            };
            let accepted_venue_id = order.venue_order_id.clone();
            let event = VenueEvent::Order(order);
            let writing = inner.writing();
            if append(&inner.journal, JournalRecord::Venue { event: event.clone() }).await.is_ok() {
                inner.report_now.notify_one();
                let mut core = inner.core.lock().await;
                let _ = core.book.apply(&event);
                inner.settle_kill(&mut core);
                // A cancel asked for while the submit was in flight had no
                // venue id to send; send it now that the venue has one.
                if !accepted_venue_id.is_empty() && core.book.cancel_requested(&task_submission.client_order_id) {
                    spawn_cancel(
                        inner.clone(),
                        task_submission.client_order_id.clone(),
                        accepted_venue_id,
                        intent.instrument.clone(),
                    );
                }
            } else {
                inner.suspend(Suspension::Journal);
            }
            drop(writing);
            if state == RuntimeOrderState::Unknown {
                spawn_resolve(
                    Arc::downgrade(&inner),
                    task_submission.client_order_id.clone(),
                    intent.instrument.clone(),
                );
            }
            inner.push_event(RuntimeEvent::SubmitOutcome {
                account: inner.identity.clone(),
                client_order_id: task_submission.client_order_id.clone(),
                outcome,
            });
        });
        Ok(submission)
    }

    pub async fn cancel(&self, client_id: &str) -> Result<(), RuntimeError> {
        cancel_on(&self.inner, client_id).await
    }

    pub async fn position(&self, instrument: &str) -> f64 {
        self.inner.core.lock().await.book.position(instrument)
    }

    pub async fn leaves(&self, client_order_id: &str) -> Option<f64> {
        self.inner.core.lock().await.book.leaves(client_order_id)
    }

    pub fn next_event(&self) -> Option<RuntimeEvent> {
        self.inner.events.lock().ok()?.pop_front()
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.inner.core.lock().await.closed = true;
        self.inner.shutdown.send_replace(true);
        let task = self.events_task.lock().ok().and_then(|mut t| t.take());
        if let Some(task) = task {
            let _ = task.await;
        }
        let tasks = self.link_tasks.lock().ok().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default();
        for task in tasks {
            let _ = task.await;
        }
        Ok(())
    }

    pub async fn journal_bytes(&self) -> u64 {
        self.inner.journal.lock().map(|j| j.byte_len()).unwrap_or(0)
    }

    pub fn fail_next_journal_write_for_test(&self) {
        if let Ok(mut journal) = self.inner.journal.lock() {
            journal.fail_next_write_for_test();
        }
    }
}


async fn apply_control_inner(inner: &Arc<Inner>, message: Received<ToRuntime>) -> Result<Vec<ControlAction>, RuntimeError> {
    if let ToRuntime::Rebaseline { rebaseline } = &message.message {
        let command = rebaseline.command_id.clone();
        inner.acknowledge(&command, ControlAckStage::Received);
        rebaseline_inventory(inner, &command).await?;
        inner.acknowledge(&command, ControlAckStage::ConfirmedClear);
        return Ok(Vec::new());
    }
    let incoming = message.message.clone();
    let command = match &incoming {
        ToRuntime::Kill { kill } => Some(kill.command_id.clone()),
        ToRuntime::Enable { enable } => Some(enable.command_id.clone()),
        _ => None,
    };
    if let Some(command) = &command {
        inner.acknowledge(command, ControlAckStage::Received);
    }
    let (actions, kill, cancellations) = {
        let mut core = inner.core.lock().await;
        let actions = core.control.apply(message)?;
        for action in &actions {
            match action {
                ControlAction::KillLatched { .. } => {
                    if let Some(command) = &command {
                        inner.acknowledge(command, ControlAckStage::AdmissionStopped);
                        core.pending_clear = Some(command.clone());
                    }
                }
                ControlAction::PolicyInstalled { revision } => {
                    inner.send(FromRuntime::PolicyAck { policy_revision: *revision });
                }
                _ => {}
            }
        }
        for action in &actions {
            match action {
                ControlAction::KillLatched { .. } => core.book.set_trading_state_halted(),
                ControlAction::KillCleared { .. } => core.book.set_trading_state_active(),
                _ => {}
            }
        }
        match &incoming {
            ToRuntime::Bootstrap { bootstrap } => {
                core.book.install_policy(
                    bootstrap.limits.clone(),
                    bootstrap.trading_state,
                    bootstrap.instrument_states.clone(),
                );
                core.book.set_allocation(
                    bootstrap.allocation.quote_asset.clone(),
                    bootstrap.allocation.max_notional,
                    bootstrap.allocation.max_position_qty,
                );
                core.book.set_fee_allowance_bps(bootstrap.allocation.fee_allowance_bps)?;
            }
            ToRuntime::Policy { policy } => {
                core.book.install_policy(
                    policy.limits.clone(),
                    policy.trading_state,
                    policy.instrument_states.clone(),
                );
            }
            _ => {}
        }
        if core.control.kill_latched().is_some() {
            core.book.set_trading_state_halted();
        }
        let kill = match &incoming {
            ToRuntime::Kill { kill } => Some(JournalRecord::Kill {
                epoch: core.control.kill_latched().unwrap_or(0),
                command_id: kill.command_id.clone(),
            }),
            ToRuntime::Enable { enable } => Some(JournalRecord::Enable {
                epoch: enable.stopped_epoch,
                command_id: enable.command_id.clone(),
            }),
            _ => None,
        };
        let mut cancellations = Vec::new();
        if actions.iter().any(|a| matches!(a, ControlAction::KillLatched { .. })) {
            // A kill pulls every resting order, whatever the instrument.
            for id in core.book.all_working_client_ids() {
                let _ = core.book.request_cancel(&id);
                cancellations.push(id);
            }
        }
        for action in &actions {
            if let ControlAction::CancelWorking { instruments } = action {
                for instrument in instruments {
                    for id in core.book.working_client_ids(instrument) {
                        let _ = core.book.request_cancel(&id);
                        cancellations.push(id);
                    }
                }
            }
        }
        (actions, kill, cancellations)
    };
    // Cancels go out before the kill is journalled: a failed journal write
    // must not leave orders resting at the venue.
    for id in cancellations {
        if let Err(e) = cancel_on(inner, &id).await {
            tracing::warn!(client_order_id = %id, error = %e, "runtime: cancel not sent");
        }
    }
    if matches!(incoming, ToRuntime::Kill { .. }) {
        if let Some(command) = &command {
            inner.acknowledge(command, ControlAckStage::CancelsSent);
        }
        let mut core = inner.core.lock().await;
        inner.settle_kill(&mut core);
    }
    if let Some(record) = kill {
        if let Err(e) = append(&inner.journal, record).await {
            inner.suspend(Suspension::Journal);
            return Err(e);
        }
        inner.report_now.notify_one();
    }
    Ok(actions)
}

/// Closes a journalled intent that was never sent: records why, releases its
/// reservation and tells the strategy. If the record cannot be written the
/// reservation is kept, since a restart would still see the bare intent.
async fn abandon(inner: &Arc<Inner>, client_order_id: &str, reason: &str) {
    let record = JournalRecord::Abandoned {
        client_order_id: client_order_id.to_string(),
        reason: reason.to_string(),
    };
    let _writing = inner.writing();
    if append(&inner.journal, record).await.is_err() {
        inner.suspend(Suspension::Journal);
        return;
    }
    inner.report_now.notify_one();
    {
        let mut core = inner.core.lock().await;
        core.book.abort_order(client_order_id);
        inner.settle_kill(&mut core);
    }
    inner.push_event(RuntimeEvent::SubmitOutcome {
        account: inner.identity.clone(),
        client_order_id: client_order_id.to_string(),
        outcome: SubmitOutcome::Rejected { reason: format!("not sent: {reason}") },
    });
}

/// How often the journal size is checked for compaction.
const COMPACT_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// Checkpoints and compacts the journal once it passes half its limit, and
/// only after the OMS has acknowledged more events: unacknowledged records
/// are kept, so compacting without new ACKs would only add checkpoints.
fn spawn_compaction(weak: std::sync::Weak<Inner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut compacted_at_ack = 0;
        let mut tick = tokio::time::interval(COMPACT_EVERY);
        loop {
            let Some(inner) = weak.upgrade() else { break };
            tokio::select! {
                _ = inner.stopped() => break,
                _ = tick.tick() => {}
            }
            let (bytes, acked) = match inner.journal.lock() {
                Ok(journal) => (journal.byte_len(), journal.acked()),
                Err(_) => break,
            };
            if bytes <= inner.journal_limit / 2 || acked <= compacted_at_ack {
                continue;
            }
            match checkpoint_and_compact(&inner).await {
                Ok(true) => {
                    compacted_at_ack = acked;
                    tracing::info!(before = bytes, "runtime: journal compacted");
                }
                // Writes in progress; the next tick tries again.
                Ok(false) => {}
                Err(e) => {
                    tracing::error!(error = %e, "runtime: journal compaction failed");
                    inner.suspend(Suspension::Journal);
                }
            }
        }
    })
}

/// Writes a checkpoint of the book and compacts. Holds the book lock
/// throughout, and checks under the journal lock that no book-changing write
/// is between its journal record and its book update; returns `false` if one
/// is, so the checkpoint never claims a record the book has not applied.
async fn checkpoint_and_compact(inner: &Arc<Inner>) -> Result<bool, RuntimeError> {
    let core = inner.core.lock().await;
    let book = core.book.checkpoint();
    let shared = inner.clone();
    let done = tokio::task::spawn_blocking(move || {
        let mut journal = shared
            .journal
            .lock()
            .map_err(|_| RuntimeError::JournalIo("journal lock poisoned".into()))?;
        if shared.writing.load(Ordering::SeqCst) != 0 {
            return Ok(false);
        }
        let cursor = journal.execution_cursor().ok_or_else(|| {
            RuntimeError::JournalIo("no checkpoint to take the execution cursor from".into())
        })?;
        journal.append_durable(&JournalRecord::Checkpoint { book, cursor })?;
        journal.compact()?;
        Ok(true)
    })
    .await
    .map_err(|e| RuntimeError::JournalIo(format!("compaction task: {e}")))?;
    drop(core);
    done
}

/// First lookup delay for an order whose submit answer was lost.
const RESOLVE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
/// Lookups never back off further than this.
const RESOLVE_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// Looks an unclear order up at the venue until its state is known, then
/// records it and lifts that order's suspension. "No such order" is not taken
/// as proof it was never placed: the venue may not show it yet, so the
/// runtime keeps looking and stays stopped.
fn spawn_resolve(weak: std::sync::Weak<Inner>, client_id: String, symbol: String) {
    tokio::spawn(async move {
        let mut delay = RESOLVE_BACKOFF;
        loop {
            let Some(inner) = weak.upgrade() else { return };
            match inner.venue.lookup(&client_id, &symbol).await {
                Ok(order) if order.state != RuntimeOrderState::Unknown => {
                    let venue_order_id = order.venue_order_id.clone();
                    let event = VenueEvent::Order(order);
                    let _writing = inner.writing();
                    if append(&inner.journal, JournalRecord::Venue { event: event.clone() }).await.is_err() {
                        inner.suspend(Suspension::Journal);
                        return;
                    }
                    inner.report_now.notify_one();
                    let mut core = inner.core.lock().await;
                    if let Err(e) = core.book.apply(&event) {
                        tracing::error!(client_order_id = %client_id, error = %e, "runtime: settled order does not match the book");
                        return;
                    }
                    inner.settle_kill(&mut core);
                    if !venue_order_id.is_empty() && core.book.cancel_requested(&client_id) {
                        spawn_cancel(inner.clone(), client_id.clone(), venue_order_id, symbol.clone());
                    }
                    drop(core);
                    inner.lift(&Suspension::UnclearOrder(client_id.clone()));
                    tracing::info!(client_order_id = %client_id, "runtime: unclear order settled by lookup");
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(client_order_id = %client_id, error = %e, "runtime: unclear order lookup failed, retrying");
                }
            }
            drop(inner);
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RESOLVE_MAX_BACKOFF);
        }
    });
}

/// Cancel attempts per order before admission is stopped.
const CANCEL_ATTEMPTS: u32 = 5;
/// First retry delay; doubles per attempt (50, 100, 200, 400 ms).
const CANCEL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Marks the order cancel-requested and sends the cancel if the venue knows
/// the order. An order still awaiting its submit answer is cancelled when the
/// acceptance arrives (see `submit`).
async fn cancel_on(inner: &Arc<Inner>, client_id: &str) -> Result<(), RuntimeError> {
    let request = inner.core.lock().await.book.request_cancel(client_id)?;
    if let Some(venue_order_id) = request.venue_order_id {
        spawn_cancel(inner.clone(), client_id.to_string(), venue_order_id, request.symbol);
    }
    Ok(())
}

/// Sends a cancel, retrying with backoff. When every attempt fails the order
/// may still be resting, so admission stops until an operator looks.
fn spawn_cancel(inner: Arc<Inner>, client_id: String, venue_order_id: String, symbol: String) {
    tokio::spawn(async move {
        let mut delay = CANCEL_BACKOFF;
        for attempt in 1..=CANCEL_ATTEMPTS {
            match inner.venue.cancel_order(&venue_order_id, &symbol).await {
                // Gone already: filled or cancelled; its report settles the book.
                Ok(()) | Err(BrokerError::NoSuchOrder(_)) => return,
                Err(e) if attempt == CANCEL_ATTEMPTS => {
                    tracing::error!(
                        client_order_id = %client_id,
                        error = %e,
                        attempts = attempt,
                        "runtime: cancel failed; order may still rest at the venue, admission stopped"
                    );
                    inner.suspend(Suspension::CancelFailed(client_id.clone()));
                    return;
                }
                Err(e) => {
                    tracing::warn!(client_order_id = %client_id, error = %e, attempt, "runtime: cancel failed, retrying");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    });
}

fn spawn_link_tasks(
    weak: std::sync::Weak<Inner>,
    link: Arc<dyn RuntimeLink>,
    reporter: Arc<AsyncMutex<Reporter>>,
    mut outbox: tokio::sync::mpsc::UnboundedReceiver<FromRuntime>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let control_weak = weak.clone();
    let control_link = link.clone();
    let control_reporter = reporter.clone();
    let control = tokio::spawn(async move {
        loop {
            let Some(inner) = control_weak.upgrade() else { break };
            let message = tokio::select! {
                _ = inner.stopped() => break,
                msg = control_link.recv_control() => match msg { Some(m) => m, None => break },
            };
            if let ToRuntime::EventAck { ack } = &message.message {
                let mut reporter = control_reporter.lock().await;
                if let Ok(mut journal) = inner.journal.lock() {
                    if let Err(e) = reporter.ack(&mut journal, ack.through_sequence) {
                        tracing::warn!(error = %e, "runtime: reporter ACK not durable");
                    }
                }
            }
            if let Err(e) = apply_control_inner(&inner, message).await {
                tracing::warn!(error = %e, "runtime: control message rejected");
            }
        }
    });
    let report_weak = weak;
    let report_link = link;
    let report = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            let Some(inner) = report_weak.upgrade() else { break };
            tokio::select! {
                biased;
                _ = inner.stopped() => break,
                // Acknowledgements and status go out ahead of event batches.
                message = outbox.recv() => {
                    let Some(message) = message else { break };
                    let envelope = reporter.lock().await.envelope(message);
                    if let Err(e) = report_link.send_events(envelope).await {
                        tracing::warn!(error = %e, "runtime: control acknowledgement not sent");
                    }
                    continue;
                }
                _ = inner.report_now.notified() => {}
                _ = tick.tick() => {}
            }
            let prepared = {
                let mut reporter = reporter.lock().await;
                let journal = match inner.journal.lock() {
                    Ok(journal) => journal,
                    Err(_) => break,
                };
                match reporter.next_batch(&journal, crate::reporter::REPORT_BATCH) {
                    Ok(Some(batch)) => match reporter.encode(&batch) {
                        Ok(envelope) => Some(envelope),
                        Err(e) => {
                            tracing::warn!(error = %e, "runtime: reporter encode failed");
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "runtime: reporter batch failed");
                        None
                    }
                }
            };
            if let Some(envelope) = prepared {
                if let Err(e) = report_link.send_events(envelope).await {
                    tracing::warn!(error = %e, "runtime: event send failed; pending stays durable");
                }
            }
        }
    });
    vec![control, report]
}

fn spawn_venue_events(
    weak: std::sync::Weak<Inner>,
    mut rx: tokio::sync::mpsc::Receiver<VenueEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else { break };
            let event = tokio::select! {
                _ = inner.stopped() => break,
                event = rx.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
            };
            drop(inner);
            let Some(inner) = weak.upgrade() else { break };
            let health = match &event {
                VenueEvent::Health { state, .. } => Some(*state),
                _ => None,
            };
            if matches!(health, Some(StreamState::Stale | StreamState::Closed)) {
                // Fills may now be missed: no new orders until a resync.
                inner.suspend(Suspension::FeedLost);
            }
            ingest(&inner, event).await;
            if health == Some(StreamState::Live) && inner.is_suspended_for(&Suspension::FeedLost) {
                spawn_resync(inner.clone());
            }
        }
    })
}

/// Journals a venue event, then applies it to the book. Returns whether the
/// book accepted it; a mismatch suspends the runtime.
async fn ingest(inner: &Arc<Inner>, event: VenueEvent) -> bool {
    let record = JournalRecord::Venue { event: event.clone() };
    let _writing = inner.writing();
    if append(&inner.journal, record).await.is_err() {
        inner.suspend(Suspension::Journal);
        return false;
    }
    inner.report_now.notify_one();
    let mut core = inner.core.lock().await;
    let applied = core.book.apply(&event);
    inner.settle_kill(&mut core);
    inner.raise_drift(&core);
    if applied.is_err() {
        core.control.suspend("venue event did not match local state");
        return false;
    }
    true
}

/// Attempts to read balances with no fill landing in between.
const REBASELINE_ATTEMPTS: usize = 3;

/// Takes the venue's current balances as the expected inventory. Refused
/// while an order's venue state is unknown; retried if a fill lands between
/// the balance read and the update, since the read may not include it.
async fn rebaseline_inventory(inner: &Arc<Inner>, command_id: &str) -> Result<(), RuntimeError> {
    for _ in 0..REBASELINE_ATTEMPTS {
        let fills_before = {
            let core = inner.core.lock().await;
            if !core.book.pending_client_ids().is_empty() {
                return Err(RuntimeError::InvalidControl(
                    "an order's venue state is unknown; rebaseline once it settles".into(),
                ));
            }
            core.book.fills_applied()
        };
        let balances = inner
            .venue
            .balances()
            .await
            .map_err(|e| RuntimeError::InvalidControl(format!("balances for rebaseline: {e}")))?;
        let _writing = inner.writing();
        let mut core = inner.core.lock().await;
        if core.book.fills_applied() != fills_before || !core.book.pending_client_ids().is_empty() {
            continue;
        }
        let mut amounts = std::collections::BTreeMap::new();
        for balance in &balances {
            *amounts.entry(balance.asset.clone()).or_insert(0.0) += balance.total();
        }
        // Journalled under the book lock, so no fill is applied before the
        // baseline it would otherwise be counted against.
        if let Err(e) = append(
            &inner.journal,
            JournalRecord::Baseline { command_id: command_id.to_string(), amounts: amounts.clone() },
        )
        .await
        {
            inner.suspend(Suspension::Journal);
            return Err(e);
        }
        core.book.install_balances(&balances);
        core.book.rebaseline_inventory(&amounts);
        inner.lift(&Suspension::BalanceDrift);
        inner.send(FromRuntime::InventoryStatus { shortfalls: Vec::new() });
        tracing::info!(command_id, "runtime: inventory rebaselined from venue balances");
        return Ok(());
    }
    Err(RuntimeError::InvalidControl("fills kept arriving; rebaseline again".into()))
}

/// How often venue balances are read and checked against the inventory.
const BALANCE_POLL: std::time::Duration = std::time::Duration::from_secs(30);

fn spawn_balance_poll(weak: std::sync::Weak<Inner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else { break };
            tokio::select! {
                _ = inner.stopped() => break,
                _ = tokio::time::sleep(BALANCE_POLL) => {}
            }
            // A resync reads balances itself, after the fills it catches up on.
            if inner.resyncing.load(Ordering::SeqCst) || inner.is_suspended_for(&Suspension::FeedLost) {
                continue;
            }
            match inner.venue.balances().await {
                Ok(balances) => {
                    ingest(&inner, VenueEvent::Balances { balances }).await;
                }
                Err(e) => tracing::warn!(error = %e, "runtime: balance poll failed"),
            }
        }
    })
}

/// History pages read per resync before giving up.
const RESYNC_MAX_PAGES: usize = 100;

/// After the feed returns, catches up on what it missed: fills since the
/// execution cursor, the open orders and the balances, each journalled and
/// applied like a live event. Only a resync that completes lifts the feed
/// suspension; anything else leaves orders stopped for the next attempt.
fn spawn_resync(inner: Arc<Inner>) {
    if inner.resyncing.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        let outcome = resync(&inner).await;
        inner.resyncing.store(false, Ordering::SeqCst);
        match outcome {
            Ok(()) => {
                inner.lift(&Suspension::FeedLost);
                tracing::info!("runtime: venue feed resynced; admission reopened");
            }
            Err(reason) => tracing::warn!(%reason, "runtime: resync after feed loss failed; orders stay stopped"),
        }
    });
}

async fn resync(inner: &Arc<Inner>) -> Result<(), String> {
    let positions = inner
        .journal
        .lock()
        .map_err(|_| "journal lock poisoned".to_string())?
        .execution_cursor()
        .ok_or("no checkpoint to resume history from")?;
    let mut cursor = gateway::runtime::ExecutionCursor { positions, ..Default::default() };
    let mut complete = false;
    for _ in 0..RESYNC_MAX_PAGES {
        let page = inner.venue.executions(&cursor).await.map_err(|e| format!("history: {e}"))?;
        if !page.missing.is_empty() {
            return Err(format!("history missing for {}", page.missing.join(", ")));
        }
        for trade in page.trades {
            if !ingest(inner, VenueEvent::Trade(trade)).await {
                return Err("a missed fill did not match the book".into());
            }
        }
        cursor = page.next;
        if page.complete {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err("history did not complete within the page bound".into());
    }
    for order in inner.venue.snapshot_orders().await.map_err(|e| format!("open orders: {e}"))? {
        if !ingest(inner, VenueEvent::Order(order)).await {
            return Err("an open order did not match the book".into());
        }
    }
    let balances = inner.venue.balances().await.map_err(|e| format!("balances: {e}"))?;
    if !ingest(inner, VenueEvent::Balances { balances }).await {
        return Err("balances could not be applied".into());
    }
    Ok(())
}

fn recovered_complete(book: &Book, control: &ControlState, bootstrap_killed: Option<u64>, journal_killed: bool) -> bool {
    bootstrap_killed.is_none() && !journal_killed && control.kill_latched().is_none() && !book.is_paused()
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.inner.shutdown.send_replace(true);
        if let Ok(mut task) = self.events_task.lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Ok(mut tasks) = self.link_tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

/// Why admission is stopped. Only `UnclearOrder` lifts itself (when a venue
/// lookup settles the order); every other reason needs a restart, because the
/// local record or the venue's state can no longer be trusted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Suspension {
    /// A journal write failed or the journal is over its limit.
    Journal,
    /// Every cancel attempt for this order failed; it may still rest.
    CancelFailed(String),
    /// The submit answer was lost; the venue may have taken this order.
    UnclearOrder(String),
    /// The strategy stopped draining runtime events.
    EventQueueFull,
    /// The venue feed went stale or closed; lifted by a completed resync.
    FeedLost,
    /// Venue balances fell short of the expected inventory; lifted only by
    /// an operator rebaseline.
    BalanceDrift,
}

impl std::fmt::Display for Suspension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Journal => write!(f, "journal write failed"),
            Self::CancelFailed(id) => write!(f, "cancel of {id} failed"),
            Self::UnclearOrder(id) => write!(f, "order {id} has no confirmed venue state"),
            Self::EventQueueFull => write!(f, "event queue is full"),
            Self::FeedLost => write!(f, "venue feed was lost; resyncing"),
            Self::BalanceDrift => write!(f, "venue balances fell short of the expected inventory"),
        }
    }
}

/// Counts one book-changing journal write until it is applied to the book.
struct Writing<'a>(&'a AtomicUsize);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Inner {
    fn writing(&self) -> Writing<'_> {
        self.writing.fetch_add(1, Ordering::SeqCst);
        Writing(&self.writing)
    }

    /// Resolves once shutdown has begun, including when it began earlier.
    async fn stopped(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|stopping| *stopping).await;
    }

    /// Queues a message for the OMS; dropped silently without a link.
    fn send(&self, message: FromRuntime) {
        if let Some(outbox) = &self.outbox {
            let _ = outbox.send(message);
        }
    }

    fn acknowledge(&self, command_id: &str, stage: ControlAckStage) {
        self.send(FromRuntime::ControlAck { epoch: self.epoch, command_id: command_id.to_string(), stage });
    }

    /// Confirms a kill clear once no order may still rest at the venue.
    fn settle_kill(&self, core: &mut Core) {
        if core.pending_clear.is_some() && core.book.all_working_client_ids().is_empty() {
            if let Some(command) = core.pending_clear.take() {
                self.acknowledge(&command, ControlAckStage::ConfirmedClear);
            }
        }
    }

    /// Suspends admission and tells the OMS the first time the book
    /// confirms an inventory shortfall.
    fn raise_drift(&self, core: &Core) {
        let Some(drift) = core.book.inventory_drift() else { return };
        if self.is_suspended_for(&Suspension::BalanceDrift) {
            return;
        }
        let shortfalls: Vec<String> = drift.iter().map(ToString::to_string).collect();
        tracing::warn!(shortfalls = ?shortfalls, "runtime: venue inventory short; orders stopped");
        self.suspend(Suspension::BalanceDrift);
        self.send(FromRuntime::InventoryStatus { shortfalls });
    }

    fn suspend(&self, reason: Suspension) {
        if let Ok(mut set) = self.suspensions.lock() {
            set.insert(reason);
        }
    }

    fn is_suspended_for(&self, reason: &Suspension) -> bool {
        self.suspensions.lock().map(|set| set.contains(reason)).unwrap_or(true)
    }

    fn lift(&self, reason: &Suspension) {
        if let Ok(mut set) = self.suspensions.lock() {
            set.remove(reason);
        }
    }

    /// `None` when admission is open. A poisoned lock counts as suspended.
    fn suspension_reasons(&self) -> Option<String> {
        match self.suspensions.lock() {
            Ok(set) if set.is_empty() => None,
            Ok(set) => Some(set.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")),
            Err(_) => Some("suspension state is poisoned".into()),
        }
    }

    fn push_event(&self, event: RuntimeEvent) {
        if let Ok(mut events) = self.events.lock() {
            if events.len() < self.capacity {
                events.push_back(event);
            } else {
                self.suspend(Suspension::EventQueueFull);
            }
        }
    }

}

async fn append(journal: &Arc<Mutex<Journal>>, record: JournalRecord) -> Result<u64, RuntimeError> {
    let journal = journal.clone();
    tokio::task::spawn_blocking(move || {
        let mut journal = journal
            .lock()
            .map_err(|_| RuntimeError::JournalIo("journal lock poisoned".into()))?;
        journal.append_durable(&record)
    })
    .await
    .map_err(|e| RuntimeError::JournalIo(format!("journal task: {e}")))?
}

// Keep these fields part of the explicit configuration contract even before
// the reporter connection is wired by the caller.
#[allow(dead_code)]
fn configured_links(_: &str, _: &str) {}
