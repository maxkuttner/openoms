//! Version 2 control/reporting protocol. Explicitly versioned envelopes bind
//! every message to a deployment/account, epoch, policy revision and session
//! nonce. Control and event traffic use separate authenticated connections;
//! v1 remains untouched for legacy runtimes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use gateway::runtime::{
    AccountIdentity, ExecutionCursor, InstrumentRules, RuntimeEvent,
};
use serde::{Deserialize, Serialize};

use crate::proto::HEARTBEAT;

pub const PROTOCOL_VERSION: u32 = 2;
/// 10%: a fee allowance above this is a configuration mistake.
pub const MAX_FEE_ALLOWANCE_BPS: f64 = 1_000.0;
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_EVENT_RECORDS: usize = 128;

/// A message timestamped when the socket yielded it, before any queueing.
#[derive(Debug, Clone, PartialEq)]
pub struct Received<T> {
    pub message: T,
    pub received_at: Instant,
}

impl<T> Received<T> {
    pub fn new(message: T, received_at: Instant) -> Self {
        Self { message, received_at }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    pub heartbeat_secs: u64,
    pub authority_secs: u64,
    pub market_data_max_age_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub sequence: u64,
    pub positions: BTreeMap<String, f64>,
    pub open_client_ids: Vec<String>,
    pub execution_cursor: ExecutionCursor,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allocation {
    pub quote_asset: String,
    pub max_notional: f64,
    pub max_position_qty: Option<f64>,
    /// Fee headroom held back on every buy, in basis points of its notional,
    /// so the last funds are not spent on an order whose fee cannot be paid.
    #[serde(default)]
    pub fee_allowance_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub instruments: Vec<InstrumentRules>,
    pub limits: BTreeMap<String, risk::Limits>,
    pub trading_state: risk::TradingState,
    #[serde(default)]
    pub instrument_states: BTreeMap<String, risk::TradingState>,
    pub timings: Timings,
    pub baseline: RuntimeCheckpoint,
    #[serde(default)]
    pub asset_baseline: BTreeMap<String, f64>,
    /// Persistent stop latch; an Active policy cannot clear it.
    #[serde(default)]
    pub killed_epoch: Option<u64>,
    pub allocation: Allocation,
    pub recovery_universe: Vec<String>,
}

impl Bootstrap {
    /// Registration refuses missing timings rather than inventing production
    /// defaults: authority and market-data age must both be explicit and
    /// positive, and authority may not be shorter than the heartbeat.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.instruments.is_empty() {
            return Err(ProtocolError::InvalidBootstrap("no instrument rules".into()));
        }
        let mut symbols = BTreeSet::new();
        for instrument in &self.instruments {
            if instrument.symbol.trim().is_empty() || !symbols.insert(instrument.symbol.clone()) {
                return Err(ProtocolError::InvalidBootstrap(
                    "instrument rules have an empty or duplicate symbol".into(),
                ));
            }
        }
        if self.limits.keys().any(|s| !symbols.contains(s)) {
            return Err(ProtocolError::InvalidBootstrap(
                "limits reference an instrument without rules".into(),
            ));
        }
        if symbols.iter().any(|symbol| !self.limits.contains_key(symbol)) {
            return Err(ProtocolError::InvalidBootstrap(
                "every instrument needs a resolved limit entry".into(),
            ));
        }
        if self.timings.heartbeat_secs == 0 {
            return Err(ProtocolError::InvalidBootstrap("heartbeat must be positive".into()));
        }
        if self.timings.authority_secs == 0 {
            return Err(ProtocolError::InvalidBootstrap("authority duration must be explicit".into()));
        }
        if self.timings.market_data_max_age_secs == 0 {
            return Err(ProtocolError::InvalidBootstrap("market-data age must be explicit".into()));
        }
        if self.timings.authority_secs < self.timings.heartbeat_secs {
            return Err(ProtocolError::AuthorityTooShort {
                secs: self.timings.authority_secs,
                heartbeat_secs: self.timings.heartbeat_secs,
            });
        }
        if !self.allocation.max_notional.is_finite() || self.allocation.max_notional <= 0.0 {
            return Err(ProtocolError::InvalidBootstrap("allocation must be positive".into()));
        }
        if !self.allocation.fee_allowance_bps.is_finite()
            || !(0.0..=MAX_FEE_ALLOWANCE_BPS).contains(&self.allocation.fee_allowance_bps)
        {
            return Err(ProtocolError::InvalidBootstrap("fee allowance must be 0 to 1000 bps".into()));
        }
        if self.allocation.quote_asset.trim().is_empty() {
            return Err(ProtocolError::InvalidBootstrap("allocation quote asset is empty".into()));
        }
        if self
            .allocation
            .max_position_qty
            .is_some_and(|q| !q.is_finite() || q <= 0.0)
        {
            return Err(ProtocolError::InvalidBootstrap("allocation position is invalid".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub revision: u64,
    pub limits: BTreeMap<String, risk::Limits>,
    pub trading_state: risk::TradingState,
    #[serde(default)]
    pub instrument_states: BTreeMap<String, risk::TradingState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Authority {
    pub valid_for_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kill {
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Enable {
    pub stopped_epoch: u64,
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventAck {
    pub through_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResyncRequired {
    pub from_sequence: u64,
    pub reason: String,
}

/// OMS → runtime control messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToRuntime {
    Bootstrap { bootstrap: Bootstrap },
    Policy { policy: Policy },
    Authority { authority: Authority },
    Kill { kill: Kill },
    Enable { enable: Enable },
    EventAck { ack: EventAck },
    ResyncRequired { resync: ResyncRequired },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToRuntimeEnvelope {
    pub protocol_version: u32,
    pub account: AccountIdentity,
    pub epoch: u64,
    pub policy_revision: u64,
    pub sequence: u64,
    pub session_nonce: String,
    pub message: ToRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAckStage {
    Received,
    AdmissionStopped,
    CancelsSent,
    ConfirmedClear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub sequence: u64,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventBatch {
    pub first_sequence: u64,
    pub records: Vec<EventRecord>,
}

impl EventBatch {
    pub fn last_sequence(&self) -> u64 {
        self.records.last().map(|r| r.sequence).unwrap_or(self.first_sequence)
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.records.len() > MAX_EVENT_RECORDS {
            return Err(ProtocolError::TooManyRecords {
                count: self.records.len(),
                max: MAX_EVENT_RECORDS,
            });
        }
        if self.records.is_empty() {
            return Err(ProtocolError::InvalidBatch("empty event batch".into()));
        }
        if self.first_sequence != self.records[0].sequence {
            return Err(ProtocolError::InvalidBatch("first sequence mismatch".into()));
        }
        for pair in self.records.windows(2) {
            if pair[1].sequence != pair[0].sequence + 1 {
                return Err(ProtocolError::SequenceGap {
                    expected: pair[0].sequence + 1,
                    got: pair[1].sequence,
                });
            }
        }
        Ok(())
    }
}

/// A control envelope as the socket yielded it: which connection it came on
/// (numbered from 1 per successful connect) and when it arrived.
#[derive(Debug, Clone)]
pub struct ControlFrame {
    pub connection: u64,
    pub received_at: Instant,
    pub envelope: ToRuntimeEnvelope,
}

/// Validates control frames for the runtime: drops frames from replaced
/// connections, requires each connection to open with its bootstrap, and runs
/// every frame through a control-only `V2Session` (version, account, nonce,
/// epoch, sequence, replay, revision and kill/enable rules). Authority is
/// timed from arrival, not from when the runtime gets to it.
pub struct ControlGate {
    session: V2Session,
    connection: u64,
}

impl ControlGate {
    pub fn new(account: AccountIdentity, epoch: u64, session_nonce: impl Into<String>) -> Self {
        Self { session: V2Session::control_only(account, epoch, session_nonce), connection: 0 }
    }

    pub fn admit(&mut self, frame: ControlFrame) -> Result<Received<ToRuntime>, ProtocolError> {
        if frame.connection < self.connection {
            return Err(ProtocolError::StaleConnection { current: self.connection, got: frame.connection });
        }
        if frame.connection > self.connection {
            if !matches!(frame.envelope.message, ToRuntime::Bootstrap { .. }) || frame.envelope.sequence != 1 {
                return Err(ProtocolError::NoBootstrap);
            }
            self.connection = frame.connection;
            self.session.new_connection();
        }
        self.session.receive_control(frame.envelope, frame.received_at)
    }

    pub fn session(&self) -> &V2Session {
        &self.session
    }
}

/// Runtime → OMS reporting messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromRuntime {
    Hello { versions: Vec<u32>, last_acked_control: u64 },
    PolicyAck { policy_revision: u64 },
    ControlAck { epoch: u64, command_id: String, stage: ControlAckStage },
    Heartbeat { last_event_sequence: u64 },
    Events { batch: EventBatch },
    RecoveryStatus { ready: bool, checkpoint: RuntimeCheckpoint, missing: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FromRuntimeEnvelope {
    pub protocol_version: u32,
    pub account: AccountIdentity,
    pub epoch: u64,
    pub policy_revision: u64,
    pub sequence: u64,
    pub session_nonce: String,
    pub message: FromRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedVersion(u32),
    WrongAccount,
    SessionMismatch,
    EpochMismatch { expected: u64, got: u64 },
    StaleControl { last: u64, got: u64 },
    ReplayConflict(u64),
    SequenceGap { expected: u64, got: u64 },
    RevisionRollback { last: u64, got: u64 },
    InvalidBootstrap(String),
    InvalidBatch(String),
    AuthorityTooShort { secs: u64, heartbeat_secs: u64 },
    AuthorityExpired,
    EnableEpochMismatch { stopped_epoch: u64, active_epoch: Option<u64> },
    NotKilled,
    AckBeyondSent { ack: u64, sent: u64 },
    AckNotABatchBoundary(u64),
    /// A message from a connection that has since been replaced.
    StaleConnection { current: u64, got: u64 },

    FrameTooLarge { bytes: usize, max: usize },
    TooManyRecords { count: usize, max: usize },
    NoBootstrap,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::WrongAccount => write!(f, "message belongs to another account"),
            Self::SessionMismatch => write!(f, "connection/session nonce mismatch"),
            Self::EpochMismatch { expected, got } => write!(f, "epoch {got} does not match {expected}"),
            Self::StaleControl { last, got } => write!(f, "control sequence {got} is older than {last}"),
            Self::ReplayConflict(seq) => write!(f, "control sequence {seq} replayed with different content"),
            Self::SequenceGap { expected, got } => write!(f, "sequence gap: expected {expected}, got {got}"),
            Self::RevisionRollback { last, got } => write!(f, "policy revision rolled back from {last} to {got}"),
            Self::InvalidBootstrap(msg) => write!(f, "invalid bootstrap: {msg}"),
            Self::InvalidBatch(msg) => write!(f, "invalid event batch: {msg}"),
            Self::AuthorityTooShort { secs, heartbeat_secs } => {
                write!(f, "authority {secs}s is shorter than heartbeat {heartbeat_secs}s")
            }
            Self::AuthorityExpired => write!(f, "authority expired before processing"),
            Self::EnableEpochMismatch { stopped_epoch, active_epoch } => {
                write!(f, "enable references stopped epoch {stopped_epoch}, active {active_epoch:?}")
            }
            Self::NotKilled => write!(f, "enable received while not killed"),
            Self::AckBeyondSent { ack, sent } => write!(f, "ACK {ack} exceeds sent {sent}"),
            Self::AckNotABatchBoundary(seq) => write!(f, "ACK {seq} is not a delivered batch boundary"),
            Self::StaleConnection { current, got } => {
                write!(f, "message from connection {got}, which connection {current} replaced")
            }
            Self::FrameTooLarge { bytes, max } => write!(f, "frame {bytes} exceeds {max}"),
            Self::TooManyRecords { count, max } => write!(f, "batch has {count} records (max {max})"),
            Self::NoBootstrap => write!(f, "bootstrap is required"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Runtime-side state machine. It validates every incoming envelope against the
/// bound session, tracks authority deadlines from socket receipt time, and
/// keeps unacknowledged event batches across reconnects.
pub struct V2Session {
    account: AccountIdentity,
    epoch: u64,
    session_nonce: String,
    last_control_sequence: u64,
    last_control: Option<(u64, ToRuntime)>,
    last_policy_revision: u64,
    last_sent_event_sequence: u64,
    last_acked_event_sequence: u64,
    pending: Vec<EventBatch>,
    authority_deadline: Option<Instant>,
    heartbeat_secs: u64,
    killed: bool,
    kill_epoch: Option<u64>,
    bootstrap: Option<Bootstrap>,
    resend_from: Option<u64>,
    /// Whether this session also sends events and so can check their ACKs.
    /// A control-only session (the runtime's link) leaves that to the reporter.
    tracks_events: bool,
}

impl V2Session {
    pub fn new(
        account: AccountIdentity,
        epoch: u64,
        session_nonce: impl Into<String>,
        heartbeat_secs: u64,
    ) -> Self {
        Self {
            account,
            epoch,
            session_nonce: session_nonce.into(),
            last_control_sequence: 0,
            last_control: None,
            last_policy_revision: 0,
            last_sent_event_sequence: 0,
            last_acked_event_sequence: 0,
            pending: Vec::new(),
            authority_deadline: None,
            heartbeat_secs: heartbeat_secs.max(1),
            killed: false,
            kill_epoch: None,
            bootstrap: None,
            resend_from: None,
            tracks_events: true,
        }
    }

    /// A session that validates control messages only; event ACKs are checked
    /// for order but their bookkeeping belongs to whoever sends the events.
    pub fn control_only(account: AccountIdentity, epoch: u64, session_nonce: impl Into<String>) -> Self {
        Self { tracks_events: false, ..Self::new(account, epoch, session_nonce, 1) }
    }

    /// A new connection numbers its control messages from 1 again, starting
    /// with its bootstrap. Authority granted on the old connection is void.
    pub fn new_connection(&mut self) {
        self.last_control_sequence = 0;
        self.last_control = None;
        self.authority_deadline = None;
    }

    /// Rebind to a new connection nonce. Buffered authority from the old
    /// connection can no longer be accepted; unacknowledged event batches stay.
    pub fn reconnect(&mut self, session_nonce: impl Into<String>) {
        self.session_nonce = session_nonce.into();
        self.authority_deadline = None;
    }

    pub fn receive_control(
        &mut self,
        envelope: ToRuntimeEnvelope,
        received_at: Instant,
    ) -> Result<Received<ToRuntime>, ProtocolError> {
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(envelope.protocol_version));
        }
        if envelope.account != self.account {
            return Err(ProtocolError::WrongAccount);
        }
        if envelope.session_nonce != self.session_nonce {
            return Err(ProtocolError::SessionMismatch);
        }
        if envelope.epoch != self.epoch {
            return Err(ProtocolError::EpochMismatch {
                expected: self.epoch,
                got: envelope.epoch,
            });
        }
        let seq = envelope.sequence;
        if seq < self.last_control_sequence {
            return Err(ProtocolError::StaleControl { last: self.last_control_sequence, got: seq });
        }
        if seq == self.last_control_sequence {
            if self.last_control.as_ref().is_some_and(|(s, m)| *s == seq && *m != envelope.message) {
                return Err(ProtocolError::ReplayConflict(seq));
            }
            if self.last_control.is_none() {
                return Err(ProtocolError::SequenceGap { expected: 1, got: seq });
            }
            if self.last_control.as_ref().is_some_and(|(_, m)| *m == envelope.message) {
                return Ok(Received::new(envelope.message, received_at));
            }
        } else if seq > self.last_control_sequence + 1 {
            return Err(ProtocolError::SequenceGap {
                expected: self.last_control_sequence + 1,
                got: seq,
            });
        }

        match &envelope.message {
            ToRuntime::Bootstrap { bootstrap } => {
                bootstrap.validate()?;
                if envelope.policy_revision < self.last_policy_revision {
                    return Err(ProtocolError::RevisionRollback {
                        last: self.last_policy_revision,
                        got: envelope.policy_revision,
                    });
                }
                self.bootstrap = Some(bootstrap.clone());
                self.heartbeat_secs = bootstrap.timings.heartbeat_secs;
                // The OMS's own record of an active kill, so a runtime that
                // restarted while killed can still be enabled.
                if let Some(killed) = bootstrap.killed_epoch {
                    self.killed = true;
                    self.kill_epoch = Some(killed);
                    self.authority_deadline = None;
                }
                self.last_policy_revision = envelope.policy_revision;
            }
            ToRuntime::Policy { .. } => {
                if envelope.policy_revision < self.last_policy_revision {
                    return Err(ProtocolError::RevisionRollback {
                        last: self.last_policy_revision,
                        got: envelope.policy_revision,
                    });
                }
                self.last_policy_revision = envelope.policy_revision;
            }
            ToRuntime::Authority { authority } => {
                if authority.valid_for_secs < self.heartbeat_secs {
                    return Err(ProtocolError::AuthorityTooShort {
                        secs: authority.valid_for_secs,
                        heartbeat_secs: self.heartbeat_secs,
                    });
                }
                let deadline = received_at
                    .checked_add(Duration::from_secs(authority.valid_for_secs))
                    .ok_or(ProtocolError::AuthorityExpired)?;
                if deadline < received_at {
                    return Err(ProtocolError::AuthorityExpired);
                }
                self.authority_deadline = Some(deadline);
            }
            ToRuntime::Kill { .. } => {
                self.killed = true;
                self.kill_epoch = Some(envelope.epoch);
                self.authority_deadline = None;
            }
            ToRuntime::Enable { enable } => {
                let active = self.kill_epoch;
                if active.is_none() {
                    return Err(ProtocolError::NotKilled);
                }
                if active != Some(enable.stopped_epoch) {
                    return Err(ProtocolError::EnableEpochMismatch {
                        stopped_epoch: enable.stopped_epoch,
                        active_epoch: active,
                    });
                }
                self.killed = false;
                self.kill_epoch = None;
            }
            ToRuntime::EventAck { ack } => {
                if self.tracks_events {
                    self.ack_events(ack.through_sequence)?;
                }
            }
            ToRuntime::ResyncRequired { .. } if !self.tracks_events => {}
            ToRuntime::ResyncRequired { resync } => {
                if resync.from_sequence > self.last_sent_event_sequence.saturating_add(1) {
                    return Err(ProtocolError::SequenceGap {
                        expected: self.last_sent_event_sequence + 1,
                        got: resync.from_sequence,
                    });
                }
                self.resend_from = Some(resync.from_sequence);
            }
        }

        self.last_control_sequence = seq;
        self.last_control = Some((seq, envelope.message.clone()));
        Ok(Received::new(envelope.message, received_at))
    }

    /// Valid only with a complete bootstrap, no kill, and unexpired authority
    /// whose receipt-time deadline has been checked while awaiting processing.
    pub fn may_trade(&self, now: Instant) -> bool {
        self.bootstrap.as_ref().is_some_and(|b| b.baseline.complete)
            && !self.killed
            && self
                .authority_deadline
                .is_some_and(|deadline| now <= deadline)
    }

    pub fn authority_deadline(&self) -> Option<Instant> {
        self.authority_deadline
    }

    pub fn heartbeat_secs(&self) -> u64 {
        self.heartbeat_secs
    }

    pub fn bootstrap(&self) -> Option<&Bootstrap> {
        self.bootstrap.as_ref()
    }

    pub fn killed(&self) -> bool {
        self.killed
    }

    pub fn seed_event_sequence(&mut self, sequence: u64) {
        self.last_sent_event_sequence = sequence;
        self.last_acked_event_sequence = sequence;
    }

    pub fn queue_events(&mut self, events: Vec<RuntimeEvent>) -> Result<EventBatch, ProtocolError> {
        if events.len() > MAX_EVENT_RECORDS {
            return Err(ProtocolError::TooManyRecords {
                count: events.len(),
                max: MAX_EVENT_RECORDS,
            });
        }
        if events.is_empty() {
            return Err(ProtocolError::InvalidBatch("empty event batch".into()));
        }
        let first_sequence = self.last_sent_event_sequence + 1;
        let records: Vec<EventRecord> = events
            .into_iter()
            .enumerate()
            .map(|(i, event)| EventRecord {
                sequence: first_sequence + i as u64,
                event,
            })
            .collect();
        let batch = EventBatch { first_sequence, records };
        batch.validate()?;
        self.last_sent_event_sequence = batch.last_sequence();
        self.pending.push(batch.clone());
        Ok(batch)
    }

    /// Wraps a non-event message (hello, acknowledgements, recovery status)
    /// in this session's envelope. Its sequence is the last event sent.
    pub fn encode_message(&self, message: FromRuntime) -> FromRuntimeEnvelope {
        FromRuntimeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            account: self.account.clone(),
            epoch: self.epoch,
            policy_revision: self.last_policy_revision,
            sequence: self.last_sent_event_sequence,
            session_nonce: self.session_nonce.clone(),
            message,
        }
    }

    pub fn encode_events(&self, batch: &EventBatch) -> Result<FromRuntimeEnvelope, ProtocolError> {
        Ok(FromRuntimeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            account: self.account.clone(),
            epoch: self.epoch,
            policy_revision: self.last_policy_revision,
            sequence: batch.last_sequence(),
            session_nonce: self.session_nonce.clone(),
            message: FromRuntime::Events { batch: batch.clone() },
        })
    }

    /// Serialize with the bounded frame rule (256 KiB, at most 128 records).
    pub fn encode_from_runtime(
        &self,
        envelope: &FromRuntimeEnvelope,
    ) -> Result<String, ProtocolError> {
        if let FromRuntime::Events { batch } = &envelope.message {
            batch.validate()?;
        }
        let text = serde_json::to_string(envelope)
            .map_err(|_| ProtocolError::InvalidBatch("serialization failed".into()))?;
        if text.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                bytes: text.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        Ok(text)
    }

    /// ACK is exact: it must name a delivered batch boundary and never exceed
    /// the last sequence sent.
    pub fn ack_events(&mut self, through_sequence: u64) -> Result<(), ProtocolError> {
        if through_sequence <= self.last_acked_event_sequence {
            return Ok(());
        }
        if through_sequence > self.last_sent_event_sequence {
            return Err(ProtocolError::AckBeyondSent {
                ack: through_sequence,
                sent: self.last_sent_event_sequence,
            });
        }
        if !self.pending.iter().any(|b| b.last_sequence() == through_sequence) {
            return Err(ProtocolError::AckNotABatchBoundary(through_sequence));
        }
        self.pending.retain(|b| b.last_sequence() > through_sequence);
        self.last_acked_event_sequence = through_sequence;
        Ok(())
    }

    pub fn pending_batches(&self) -> &[EventBatch] {
        &self.pending
    }

    pub fn resend_pending(&self) -> Vec<EventBatch> {
        self.pending.clone()
    }

    pub fn last_acked_event_sequence(&self) -> u64 {
        self.last_acked_event_sequence
    }

    pub fn last_sent_event_sequence(&self) -> u64 {
        self.last_sent_event_sequence
    }

    pub fn last_control_sequence(&self) -> u64 {
        self.last_control_sequence
    }

    pub fn last_policy_revision(&self) -> u64 {
        self.last_policy_revision
    }

    pub fn resend_from(&self) -> Option<u64> {
        self.resend_from
    }
}

/// OMS-side event inbox: detects gaps, ignores a fully duplicated batch, and
/// accepts contiguous batches only.
#[derive(Debug, Clone)]
pub struct EventInbox {
    next_sequence: u64,
}

impl EventInbox {
    pub fn new(first_sequence: u64) -> Self {
        Self { next_sequence: first_sequence }
    }

    pub fn accept(&mut self, batch: &EventBatch) -> Result<Vec<EventRecord>, ProtocolError> {
        batch.validate()?;
        if batch.last_sequence() < self.next_sequence {
            return Ok(Vec::new());
        }
        if batch.first_sequence < self.next_sequence {
            return Err(ProtocolError::SequenceGap {
                expected: self.next_sequence,
                got: batch.first_sequence,
            });
        }
        if batch.first_sequence > self.next_sequence {
            return Err(ProtocolError::SequenceGap {
                expected: self.next_sequence,
                got: batch.first_sequence,
            });
        }
        self.next_sequence = batch.last_sequence() + 1;
        Ok(batch.records.clone())
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }
}

pub fn heartbeat_interval() -> Duration {
    HEARTBEAT
}

/// The runtime's transport seam. The caller constructs the authenticated link
/// and credentials; the runtime sees only this trait and never stores secrets.
#[async_trait::async_trait]
pub trait RuntimeLink: Send + Sync {
    /// Next control message, stamped when the socket yielded it.
    async fn recv_control(&self) -> Option<Received<ToRuntime>>;
    async fn send_events(&self, envelope: FromRuntimeEnvelope) -> Result<(), ProtocolError>;
}
