//! Bounded journal-to-link reporting: batches stop at the last durable ACK and
//! replay after reconnect until the OMS acknowledges them.

use colo_link::v2::{EventBatch, FromRuntimeEnvelope, ProtocolError, V2Session};
use gateway::runtime::{AccountIdentity, RuntimeEvent};

use crate::journal::{Journal, JournalRecord};
use crate::RuntimeError;

pub const REPORT_BATCH: usize = 128;

pub struct Reporter {
    session: V2Session,
    identity: AccountIdentity,
    last_acked: u64,
    journal_acked: u64,
    sent_journal: u64,
    journal_boundaries: std::collections::BTreeMap<u64, u64>,
}

impl Reporter {
    pub fn new(
        identity: AccountIdentity,
        epoch: u64,
        session_nonce: impl Into<String>,
        _policy_revision: u64,
        journal: &Journal,
    ) -> Result<Self, RuntimeError> {
        let records = journal.replay()?;
        let acked = journal.acked();
        let sent = records
            .iter()
            .filter(|r| {
                r.sequence <= acked
                    && !matches!(
                        r.record,
                        JournalRecord::Ack { .. } | JournalRecord::Cursor { .. } | JournalRecord::Checkpoint { .. }
                    )
            })
            .count() as u64;
        let mut session = V2Session::new(identity.clone(), epoch, session_nonce, 1);
        session.seed_event_sequence(sent);
        Ok(Self {
            session,
            identity,
            last_acked: sent,
            journal_acked: acked,
            sent_journal: acked,
            journal_boundaries: std::collections::BTreeMap::new(),
        })
    }

    pub fn reconnect(&mut self, session_nonce: impl Into<String>) {
        self.session.reconnect(session_nonce);
    }

    /// New unacknowledged records as a bounded protocol batch.
    pub fn next_batch(
        &mut self,
        journal: &Journal,
        limit: usize,
    ) -> Result<Option<EventBatch>, RuntimeError> {
        if let Some(pending) = self.session.resend_pending().first() {
            return Ok(Some(pending.clone()));
        }
        let limit = limit.min(REPORT_BATCH);
        let records = journal.unacked(self.sent_journal.saturating_add(1), limit)?;
        let journal_last = records.last().map(|r| r.sequence).unwrap_or(self.journal_acked);
        let events: Vec<RuntimeEvent> = records
            .into_iter()
            .filter_map(|record| runtime_event(&record.record, &self.identity))
            .collect();
        if events.is_empty() {
            return Ok(None);
        }
        let batch = self
            .session
            .queue_events(events)
            .map_err(|e| RuntimeError::InvalidControl(e.to_string()))?;
        self.sent_journal = journal_last;
        self.journal_boundaries.insert(batch.last_sequence(), journal_last);
        Ok(Some(batch))
    }

    pub fn encode(&self, batch: &EventBatch) -> Result<FromRuntimeEnvelope, RuntimeError> {
        self.session
            .encode_events(batch)
            .map_err(|e| RuntimeError::InvalidControl(e.to_string()))
    }

    pub fn encode_bounded(&self, batch: &EventBatch) -> Result<String, RuntimeError> {
        let envelope = self.encode(batch)?;
        self.session
            .encode_from_runtime(&envelope)
            .map_err(|e| RuntimeError::InvalidControl(e.to_string()))
    }

    /// Durably record the OMS ACK and release the corresponding journal tail.
    pub fn ack(&mut self, journal: &mut Journal, through: u64) -> Result<(), RuntimeError> {
        if through <= self.last_acked {
            return Ok(());
        }
        self.session
            .ack_events(through)
            .map_err(|e| RuntimeError::InvalidControl(e.to_string()))?;
        let journal_through = self
            .journal_boundaries
            .get(&through)
            .copied()
            .ok_or_else(|| RuntimeError::InvalidControl("ACK has no journal boundary".into()))?;
        journal.ack_through(journal_through)?;
        self.journal_boundaries.retain(|protocol, _| *protocol > through);
        self.last_acked = through;
        self.journal_acked = journal_through;
        self.sent_journal = self.sent_journal.max(journal_through);
        Ok(())
    }

    pub fn last_acked(&self) -> u64 {
        self.last_acked
    }

    pub fn identity(&self) -> &AccountIdentity {
        &self.identity
    }
}

fn runtime_event(record: &JournalRecord, account: &AccountIdentity) -> Option<RuntimeEvent> {
    match record {
        JournalRecord::Intent { submission, intent, .. } => Some(RuntimeEvent::Admitted {
            account: account.clone(),
            intent: gateway::runtime::OrderIntent {
                client_order_id: submission.client_order_id.clone(),
                symbol: intent.instrument.clone(),
                side: intent.side,
                quantity: intent.quantity,
                order_type: intent.order_type,
                limit_price: intent.limit_price,
                time_in_force: intent.time_in_force,
            },
        }),
        JournalRecord::Venue { event } => Some(RuntimeEvent::Venue {
            account: account.clone(),
            event: event.clone(),
        }),
        JournalRecord::Kill { epoch, .. } => Some(RuntimeEvent::Control {
            account: account.clone(),
            command: gateway::runtime::ControlCommand::Kill,
            epoch: *epoch,
            applied: true,
        }),
        JournalRecord::Enable { epoch, .. } => Some(RuntimeEvent::Control {
            account: account.clone(),
            command: gateway::runtime::ControlCommand::Enable,
            epoch: *epoch,
            applied: true,
        }),
        JournalRecord::Policy { epoch, .. } => Some(RuntimeEvent::Control {
            account: account.clone(),
            command: gateway::runtime::ControlCommand::Policy,
            epoch: *epoch,
            applied: true,
        }),
        JournalRecord::Abandoned { client_order_id, reason } => Some(RuntimeEvent::SubmitOutcome {
            account: account.clone(),
            client_order_id: client_order_id.clone(),
            outcome: gateway::runtime::SubmitOutcome::Rejected { reason: format!("not sent: {reason}") },
        }),
        JournalRecord::Ack { .. } | JournalRecord::Cursor { .. } | JournalRecord::Checkpoint { .. } => None,
    }
}

impl From<ProtocolError> for RuntimeError {
    fn from(value: ProtocolError) -> Self {
        RuntimeError::InvalidControl(value.to_string())
    }
}
