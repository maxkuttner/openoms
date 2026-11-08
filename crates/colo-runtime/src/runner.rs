//! Embedded runtime orchestration: recovery, serialized admission, durable
//! journal write before send, and a runner that never holds the risk lock
//! across a venue call.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use colo_link::v2::{Bootstrap, Received, ToRuntime};
use gateway::adapter::BrokerError;
use gateway::runtime::{
    AccountIdentity, RuntimeEvent, RuntimeOrder, RuntimeOrderState, RuntimeVenue, SubmitOutcome,
    VenueEvent,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::book::{Book, InstrumentPrices, Intent, Prices, Submission};
use crate::control::{ControlAction, ControlState, RuntimeState};
use crate::journal::{Journal, JournalRecord};
use crate::recovery::recover;
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
    suspended: AtomicBool,
}

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
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
        let recovered = recover(&*venue, &mut journal, &config.bootstrap).await?;
        let mut control = ControlState::new(config.epoch);
        let now = Instant::now();
        control.apply(Received::new(
            ToRuntime::Bootstrap { bootstrap: config.bootstrap.clone() },
            now,
        ))?;
        control.begin_recovery();
        if recovered.complete && config.bootstrap.killed_epoch.is_none() {
            control.mark_ready();
        } else {
            control.suspend("recovery is incomplete");
        }
        let mut book = recovered.book;
        book.restore_sequence(journal.last_sequence());
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
            suspended: AtomicBool::new(false),
        });
        Ok(Self { inner })
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
        let (actions, kill) = {
            let mut core = self.inner.core.lock().await;
            let actions = core.control.apply(message)?;
            for action in &actions {
                match action {
                    ControlAction::KillLatched { .. } => core.book.set_trading_state_halted(),
                    ControlAction::KillCleared { .. } => core.book.set_trading_state_active(),
                    _ => {}
                }
            }
            let kill = actions.iter().find_map(|a| match a {
                ControlAction::KillLatched { epoch } => Some(JournalRecord::Kill {
                    epoch: *epoch,
                    command_id: format!("kill-{epoch}"),
                }),
                ControlAction::KillCleared { stopped_epoch } => Some(JournalRecord::Enable {
                    epoch: *stopped_epoch,
                    command_id: format!("enable-{stopped_epoch}"),
                }),
                _ => None,
            });
            (actions, kill)
        };
        if let Some(record) = kill {
            if let Err(e) = append(&self.inner.journal, record).await {
                self.inner.suspended.store(true, Ordering::SeqCst);
                return Err(e);
            }
        }
        Ok(actions)
    }

    pub async fn submit(&self, intent: Intent) -> Result<Submission, RuntimeError> {
        let now = Instant::now();
        let prepared = {
            let mut core = self.inner.core.lock().await;
            if core.closed || self.inner.suspended.load(Ordering::SeqCst) {
                return Err(RuntimeError::NotReady("runtime is suspended".into()));
            }
            core.control.may_submit(now)?;
            let prices = core.prices.clone();
            let prepared = core.book.prepare(intent, &prices, now)?;
            prepared
        };
        let bytes = self.inner.journal.lock().map_err(|_| RuntimeError::JournalIo("journal lock".into()))?.byte_len();
        if bytes > self.inner.journal_limit {
            self.inner.suspended.store(true, Ordering::SeqCst);
            return Err(RuntimeError::NotReady("journal is over its limit".into()));
        }
        let record = JournalRecord::Intent {
            submission: prepared.submission.clone(),
            intent: prepared.intent.clone(),
            reservation: prepared.reservation.clone(),
        };
        append(&self.inner.journal, record).await?;

        {
            let core = self.inner.core.lock().await;
            if let Err(e) = core.control.may_submit(Instant::now()) {
                self.inner.suspended.store(true, Ordering::SeqCst);
                return Err(e);
            }
        }

        let request = prepared.broker_request.clone();
        let submission = prepared.submission.clone();
        let task_submission = submission.clone();
        let intent = prepared.intent.clone();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let result = inner.venue.submit_order(&request).await;
            let (state, venue_id, outcome) = match result {
                Ok(response) => {
                    let venue_order_id = response.external_order_id.clone();
                    (
                        RuntimeOrderState::New,
                        response.external_order_id,
                        SubmitOutcome::Accepted { venue_order_id },
                    )
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
                inner.suspended.store(true, Ordering::SeqCst);
            }
            let order = RuntimeOrder {
                client_order_id: task_submission.client_order_id.clone(),
                venue_order_id: venue_id,
                symbol: intent.instrument.clone(),
                side: intent.side,
                original_quantity: intent.quantity,
                executed_quantity: if state == RuntimeOrderState::Rejected { intent.quantity } else { 0.0 },
                leaves_quantity: if state == RuntimeOrderState::Rejected { 0.0 } else { intent.quantity },
                limit_price: intent.limit_price,
                state,
                original_time: Utc::now(),
            };
            let event = VenueEvent::Order(order);
            if append(&inner.journal, JournalRecord::Venue { event: event.clone() }).await.is_ok() {
                let mut core = inner.core.lock().await;
                let _ = core.book.apply(&event);
            } else {
                inner.suspended.store(true, Ordering::SeqCst);
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
        let request = self.inner.core.lock().await.book.request_cancel(client_id)?;
        if let Some(venue_order_id) = request.venue_order_id {
            let venue = self.inner.venue.clone();
            let symbol = request.symbol;
            tokio::spawn(async move {
                let _ = venue.cancel_order(&venue_order_id, &symbol).await;
            });
        }
        Ok(())
    }

    pub fn next_event(&self) -> Option<RuntimeEvent> {
        self.inner.events.lock().ok()?.pop_front()
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.inner.core.lock().await.closed = true;
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

impl Inner {
    fn push_event(&self, event: RuntimeEvent) {
        if let Ok(mut events) = self.events.lock() {
            if events.len() < self.capacity {
                events.push_back(event);
            } else {
                self.suspended.store(true, Ordering::SeqCst);
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
