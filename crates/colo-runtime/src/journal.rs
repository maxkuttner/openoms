//! Durable local journal: length-delimited, checksummed records with a header,
//! monotonic sequences and an atomic compaction. A torn final append is
//! recoverable; complete-length corruption is not.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use gateway::runtime::{AccountIdentity, RuntimeOrderState, VenueEvent};
use serde::{Deserialize, Serialize};

use crate::book::{Intent, Submission};
use crate::assets::Reservation;
use crate::RuntimeError;

const MAGIC: &[u8; 8] = b"COLOJNL\x01";
const SCHEMA_VERSION: u32 = 1;
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    Intent {
        submission: Submission,
        intent: Intent,
        reservation: Reservation,
    },
    Venue {
        event: VenueEvent,
    },
    Policy {
        policy_id: String,
        revision: u64,
        epoch: u64,
    },
    Kill {
        epoch: u64,
        command_id: String,
    },
    Enable {
        epoch: u64,
        command_id: String,
    },
    /// Durable acknowledgement that OMS events through `through` were applied.
    Ack {
        through: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequencedRecord {
    pub schema_version: u32,
    pub sequence: u64,
    pub record: JournalRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileHeader {
    schema_version: u32,
    identity: AccountIdentity,
    /// High-water mark, so compaction never makes sequence numbers reusable.
    last_sequence: u64,
}

pub struct Journal {
    file: File,
    path: PathBuf,
    identity: AccountIdentity,
    records: Vec<SequencedRecord>,
    acked: u64,
    last_sequence: u64,
    fail_next_write: bool,
}

impl Journal {
    pub fn open(path: &Path, identity: &AccountIdentity) -> Result<Self, RuntimeError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| RuntimeError::JournalIo(format!("create journal directory: {e}")))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(|e| RuntimeError::JournalIo(format!("open journal: {e}")))?;
        file.try_lock_exclusive().map_err(|e| {
            RuntimeError::JournalIo(format!("journal is already owned by another writer: {e}"))
        })?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| RuntimeError::JournalIo(format!("read journal: {e}")))?;

        let (records, acked, last_sequence, valid_end) = if bytes.is_empty() {
            write_header(&mut file, identity, 0)?;
            sync_parent(path);
            (Vec::new(), 0, 0, file.metadata().map(|m| m.len() as usize).unwrap_or(0))
        } else {
            parse_file(&bytes, identity)?
        };

        if valid_end < bytes.len() {
            file.set_len(valid_end as u64)
                .map_err(|e| RuntimeError::JournalIo(format!("truncate torn tail: {e}")))?;
            file.sync_all()
                .map_err(|e| RuntimeError::JournalIo(format!("sync repaired journal: {e}")))?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|e| RuntimeError::JournalIo(format!("seek journal: {e}")))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            identity: identity.clone(),
            records,
            acked,
            last_sequence,
            fail_next_write: false,
        })
    }

    pub fn identity(&self) -> &AccountIdentity {
        &self.identity
    }

    pub fn append_durable(&mut self, record: &JournalRecord) -> Result<u64, RuntimeError> {
        if self.fail_next_write {
            self.fail_next_write = false;
            return Err(RuntimeError::JournalIo("injected write failure".into()));
        }
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| RuntimeError::CorruptState("journal sequence exhausted".into()))?;
        let sequenced = SequencedRecord {
            schema_version: SCHEMA_VERSION,
            sequence,
            record: record.clone(),
        };
        let payload = serde_json::to_vec(&sequenced)
            .map_err(|e| RuntimeError::JournalIo(format!("serialize journal record: {e}")))?;
        let frame = encode_frame(&payload);

        self.file
            .seek(SeekFrom::End(0))
            .map_err(|e| RuntimeError::JournalIo(format!("seek journal: {e}")))?;
        self.file
            .write_all(&frame)
            .map_err(|e| RuntimeError::JournalIo(format!("append journal: {e}")))?;
        self.file
            .flush()
            .map_err(|e| RuntimeError::JournalIo(format!("flush journal: {e}")))?;
        self.file
            .sync_all()
            .map_err(|e| RuntimeError::JournalIo(format!("sync journal: {e}")))?;

        self.records.push(sequenced);
        self.last_sequence = sequence;
        Ok(sequence)
    }

    pub fn replay(&self) -> Result<Vec<SequencedRecord>, RuntimeError> {
        Ok(self.records.clone())
    }

    pub fn ack_through(&mut self, sequence: u64) -> Result<(), RuntimeError> {
        if sequence > self.last_sequence {
            return Err(RuntimeError::CorruptState(format!(
                "ACK {sequence} exceeds the last stored event {}",
                self.last_sequence
            )));
        }
        if sequence <= self.acked {
            return Ok(());
        }
        self.append_durable(&JournalRecord::Ack { through: sequence })?;
        self.acked = sequence;
        Ok(())
    }

    pub fn unacked(&self, from: u64, limit: usize) -> Result<Vec<SequencedRecord>, RuntimeError> {
        let start = from.max(self.acked.saturating_add(1)).max(1);
        Ok(self
            .records
            .iter()
            .filter(|r| r.sequence >= start && !matches!(r.record, JournalRecord::Ack { .. }))
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn acked(&self) -> u64 {
        self.acked
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Latest kill/enable control record wins. Compaction preserves the chain.
    pub fn kill_latched(&self) -> bool {
        let mut latched = false;
        for record in &self.records {
            match record.record {
                JournalRecord::Kill { .. } => latched = true,
                JournalRecord::Enable { .. } => latched = false,
                _ => {}
            }
        }
        latched
    }

    /// Rewrite the journal atomically, retaining unacknowledged events, live or
    /// unknown orders, the latest policy/ACK watermark and the kill/enable chain.
    pub fn compact(&mut self) -> Result<(), RuntimeError> {
        let live = live_orders(&self.records);
        let latest_ack = self
            .records
            .iter()
            .filter(|r| matches!(r.record, JournalRecord::Ack { .. }))
            .map(|r| r.sequence)
            .max();
        let latest_policy = self
            .records
            .iter()
            .filter(|r| matches!(r.record, JournalRecord::Policy { .. }))
            .map(|r| r.sequence)
            .max();

        let keep: Vec<SequencedRecord> = self
            .records
            .iter()
            .filter(|r| {
                r.sequence > self.acked
                    || matches!(r.record, JournalRecord::Kill { .. } | JournalRecord::Enable { .. })
                    || Some(r.sequence) == latest_ack
                    || Some(r.sequence) == latest_policy
                    || match &r.record {
                        JournalRecord::Intent { submission, .. } => live.contains(&submission.client_order_id),
                        JournalRecord::Venue { event } => event_client(event).is_some_and(|id| live.contains(id)),
                        _ => false,
                    }
            })
            .cloned()
            .collect();

        let tmp = self.path.with_file_name(format!(
            "{}.compact",
            self.path.file_name().and_then(|n| n.to_str()).unwrap_or("journal")
        ));
        let _ = fs::remove_file(&tmp);
        let mut tmp_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| RuntimeError::JournalIo(format!("create compacted journal: {e}")))?;
        tmp_file.try_lock_exclusive().map_err(|e| {
            RuntimeError::JournalIo(format!("lock compacted journal: {e}"))
        })?;

        let header = FileHeader {
            schema_version: SCHEMA_VERSION,
            identity: self.identity.clone(),
            last_sequence: self.last_sequence,
        };
        let header_payload = serde_json::to_vec(&header)
            .map_err(|e| RuntimeError::JournalIo(format!("serialize journal header: {e}")))?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&encode_frame(&header_payload));
        for record in &keep {
            let payload = serde_json::to_vec(record)
                .map_err(|e| RuntimeError::JournalIo(format!("serialize journal record: {e}")))?;
            bytes.extend_from_slice(&encode_frame(&payload));
        }
        tmp_file
            .write_all(&bytes)
            .map_err(|e| RuntimeError::JournalIo(format!("write compacted journal: {e}")))?;
        tmp_file
            .flush()
            .map_err(|e| RuntimeError::JournalIo(format!("flush compacted journal: {e}")))?;
        tmp_file
            .sync_all()
            .map_err(|e| RuntimeError::JournalIo(format!("sync compacted journal: {e}")))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| RuntimeError::JournalIo(format!("replace journal: {e}")))?;
        sync_parent(&self.path);
        self.file = tmp_file;
        self.records = keep;
        Ok(())
    }

    /// Test-only: make the next append fail before writing anything.
    #[doc(hidden)]
    pub fn fail_next_write_for_test(&mut self) {
        self.fail_next_write = true;
    }
}

fn write_header(file: &mut File, identity: &AccountIdentity, last_sequence: u64) -> Result<(), RuntimeError> {
    let header = FileHeader {
        schema_version: SCHEMA_VERSION,
        identity: identity.clone(),
        last_sequence,
    };
    let payload = serde_json::to_vec(&header)
        .map_err(|e| RuntimeError::JournalIo(format!("serialize journal header: {e}")))?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&encode_frame(&payload));
    file.seek(SeekFrom::Start(0))
        .map_err(|e| RuntimeError::JournalIo(format!("seek journal header: {e}")))?;
    file.write_all(&bytes)
        .map_err(|e| RuntimeError::JournalIo(format!("write journal header: {e}")))?;
    file.flush()
        .map_err(|e| RuntimeError::JournalIo(format!("flush journal header: {e}")))?;
    file.sync_all()
        .map_err(|e| RuntimeError::JournalIo(format!("sync journal header: {e}")))?;
    Ok(())
}

fn parse_file(
    bytes: &[u8],
    expected: &AccountIdentity,
) -> Result<(Vec<SequencedRecord>, u64, u64, usize), RuntimeError> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(RuntimeError::CorruptState("journal magic is missing".into()));
    }
    let mut offset = MAGIC.len();
    let header = match frame_at(bytes, offset)? {
        Frame::Complete { payload, end } => {
            let header: FileHeader = serde_json::from_slice(payload)
                .map_err(|e| RuntimeError::CorruptState(format!("journal header: {e}")))?;
            offset = end;
            header
        }
        Frame::Torn => {
            return Err(RuntimeError::CorruptState("journal header is torn".into()));
        }
    };
    if header.schema_version != SCHEMA_VERSION {
        return Err(RuntimeError::CorruptState(format!(
            "unsupported journal schema {}",
            header.schema_version
        )));
    }
    if header.identity != *expected {
        return Err(RuntimeError::CorruptState(
            "journal belongs to a different account identity".into(),
        ));
    }

    let mut records = Vec::new();
    let mut previous = 0u64;
    let mut valid_end = offset;
    while offset < bytes.len() {
        match frame_at(bytes, offset)? {
            Frame::Torn => break,
            Frame::Complete { payload, end } => {
                let record: SequencedRecord = serde_json::from_slice(payload)
                    .map_err(|e| RuntimeError::CorruptState(format!("journal record: {e}")))?;
                if record.schema_version != SCHEMA_VERSION {
                    return Err(RuntimeError::CorruptState(format!(
                        "unsupported record schema {}",
                        record.schema_version
                    )));
                }
                if record.sequence <= previous {
                    return Err(RuntimeError::CorruptState(
                        "journal sequence is not monotonic".into(),
                    ));
                }
                previous = record.sequence;
                records.push(record);
                offset = end;
                valid_end = end;
            }
        }
    }

    let acked = records
        .iter()
        .filter_map(|r| match r.record {
            JournalRecord::Ack { through } => Some(through),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let last_sequence = header
        .last_sequence
        .max(records.last().map(|r| r.sequence).unwrap_or(0));
    Ok((records, acked, last_sequence, valid_end))
}

enum Frame<'a> {
    Complete { payload: &'a [u8], end: usize },
    Torn,
}

fn frame_at(bytes: &[u8], offset: usize) -> Result<Frame<'_>, RuntimeError> {
    let remaining = bytes.len().saturating_sub(offset);
    if remaining < 8 {
        return Ok(Frame::Torn);
    }
    let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes")) as usize;
    if len > MAX_FRAME {
        return Err(RuntimeError::CorruptState("journal frame is too large".into()));
    }
    let end = offset + 8 + len;
    if end > bytes.len() {
        return Ok(Frame::Torn);
    }
    let checksum = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().expect("4 bytes"));
    let payload = &bytes[offset + 8..end];
    if crc32fast::hash(payload) != checksum {
        return Err(RuntimeError::CorruptState("journal checksum mismatch".into()));
    }
    Ok(Frame::Complete { payload, end })
}

fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn live_orders(records: &[SequencedRecord]) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    for record in records {
        match &record.record {
            JournalRecord::Intent { submission, .. } => {
                live.insert(submission.client_order_id.clone());
            }
            JournalRecord::Venue { event } => {
                if let VenueEvent::Order(order) = event {
                    if is_terminal(order.state) && !order.client_order_id.is_empty() {
                        live.remove(&order.client_order_id);
                    }
                }
            }
            _ => {}
        }
    }
    live
}

fn event_client(event: &VenueEvent) -> Option<&str> {
    match event {
        VenueEvent::Order(order) if !order.client_order_id.is_empty() => Some(&order.client_order_id),
        VenueEvent::Trade(trade) if !trade.client_order_id.is_empty() => Some(&trade.client_order_id),
        _ => None,
    }
}

fn is_terminal(state: RuntimeOrderState) -> bool {
    matches!(
        state,
        RuntimeOrderState::Filled
            | RuntimeOrderState::Canceled
            | RuntimeOrderState::Rejected
            | RuntimeOrderState::Expired
    )
}
