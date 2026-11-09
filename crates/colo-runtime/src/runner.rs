//! Embedded runtime orchestration: recovery, serialized admission, durable
//! journal write before send, and a runner that never holds the risk lock
//! across a venue call.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use colo_link::v2::{Bootstrap, Received, RuntimeLink, ToRuntime};
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
}

struct Inner {
    core: AsyncMutex<Core>,
    journal: Arc<Mutex<Journal>>,
    venue: Arc<dyn RuntimeVenue>,
    events: Mutex<VecDeque<RuntimeEvent>>,
    capacity: usize,
    journal_limit: u64,
    identity: AccountIdentity,
    suspensions: Mutex<std::collections::BTreeSet<Suspension>>,
    shutdown: tokio::sync::Notify,
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
        let RecoveredBook { mut book, events, complete: recovered_ok, .. } = recovered;
        book.restore_sequence(journal.last_sequence());
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
        let inner = Arc::new(Inner {
            core: AsyncMutex::new(Core {
                book,
                control,
                prices: Prices::new(),
                closed: false,
            }),
            journal: Arc::new(Mutex::new(journal)),
            venue,
            events: Mutex::new(VecDeque::new()),
            capacity: config.event_queue_capacity,
            journal_limit: config.max_journal_bytes,
            identity: config.identity,
            suspensions: Mutex::new(std::collections::BTreeSet::new()),
            shutdown: tokio::sync::Notify::new(),
            report_now: tokio::sync::Notify::new(),
            recovered: AtomicBool::new(recovered_ok),
        });
        let task = {
            let weak = Arc::downgrade(&inner);
            spawn_venue_events(weak, events)
        };
        let link_tasks = match (&config.link, reporter) {
            (Some(link), Some(reporter)) => {
                spawn_link_tasks(Arc::downgrade(&inner), link.clone(), reporter)
            }
            _ => Vec::new(),
        };
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
            core.book.prepare(intent, &prices, now)?
        };
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
        self.inner.report_now.notify_one();

        {
            let core = self.inner.core.lock().await;
            if let Err(e) = core.control.may_submit(Instant::now()) {
                self.inner.suspend(Suspension::LapsedAfterIntent);
                return Err(e);
            }
        }

        let request = prepared.broker_request.clone();
        let submission = prepared.submission.clone();
        let task_submission = submission.clone();
        let intent = prepared.intent.clone();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            {
                let core = inner.core.lock().await;
                if core.book.cancel_requested(&task_submission.client_order_id) {
                    inner.push_event(RuntimeEvent::SubmitOutcome {
                        account: inner.identity.clone(),
                        client_order_id: task_submission.client_order_id,
                        outcome: SubmitOutcome::Unknown { reason: "cancelled before send".into() },
                    });
                    return;
                }
                if core.control.may_submit(Instant::now()).is_err() {
                    inner.push_event(RuntimeEvent::SubmitOutcome {
                        account: inner.identity.clone(),
                        client_order_id: task_submission.client_order_id,
                        outcome: SubmitOutcome::Unknown { reason: "admission stopped before send".into() },
                    });
                    return;
                }
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
            if append(&inner.journal, JournalRecord::Venue { event: event.clone() }).await.is_ok() {
                inner.report_now.notify_one();
                let mut core = inner.core.lock().await;
                let _ = core.book.apply(&event);
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
        self.inner.shutdown.notify_waiters();
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
    let incoming = message.message.clone();
    let (actions, kill, cancellations) = {
        let mut core = inner.core.lock().await;
        let actions = core.control.apply(message)?;
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
    if let Some(record) = kill {
        if let Err(e) = append(&inner.journal, record).await {
            inner.suspend(Suspension::Journal);
            return Err(e);
        }
        inner.report_now.notify_one();
    }
    Ok(actions)
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
) -> Vec<tokio::task::JoinHandle<()>> {
    let control_weak = weak.clone();
    let control_link = link.clone();
    let control_reporter = reporter.clone();
    let control = tokio::spawn(async move {
        loop {
            let Some(inner) = control_weak.upgrade() else { break };
            let message = tokio::select! {
                _ = inner.shutdown.notified() => break,
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
                _ = inner.shutdown.notified() => break,
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
                    _ = inner.shutdown.notified() => break,
                    event = rx.recv() => match event {
                        Some(event) => event,
                        None => break,
                    },
                };
                drop(inner);
                let Some(inner) = weak.upgrade() else { break };
                if let VenueEvent::Health { state, .. } = &event {
                    if matches!(state, StreamState::Stale | StreamState::Closed) {
                        let mut core = inner.core.lock().await;
                        core.control.suspend("venue stream is not healthy");
                    }
                }
                let record = JournalRecord::Venue { event: event.clone() };
                if append(&inner.journal, record).await.is_err() {
                    inner.suspend(Suspension::Journal);
                    continue;
                }
                inner.report_now.notify_one();
                let mut core = inner.core.lock().await;
                if core.book.apply(&event).is_err() {
                    core.control.suspend("venue event did not match local state");
                }
            }
        })
}

fn recovered_complete(book: &Book, control: &ControlState, bootstrap_killed: Option<u64>, journal_killed: bool) -> bool {
    bootstrap_killed.is_none() && !journal_killed && control.kill_latched().is_none() && !book.is_paused()
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.inner.shutdown.notify_waiters();
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
    /// Admission closed between journalling an intent and sending it.
    LapsedAfterIntent,
    /// The strategy stopped draining runtime events.
    EventQueueFull,
}

impl std::fmt::Display for Suspension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Journal => write!(f, "journal write failed"),
            Self::CancelFailed(id) => write!(f, "cancel of {id} failed"),
            Self::UnclearOrder(id) => write!(f, "order {id} has no confirmed venue state"),
            Self::LapsedAfterIntent => write!(f, "admission closed after an intent was journalled"),
            Self::EventQueueFull => write!(f, "event queue is full"),
        }
    }
}

impl Inner {
    fn suspend(&self, reason: Suspension) {
        if let Ok(mut set) = self.suspensions.lock() {
            set.insert(reason);
        }
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
