use std::fs;
use std::path::Path;

use chrono::{TimeZone, Utc};
use colo_runtime::journal::Journal;
use colo_runtime::{
    AccountIdentity, Intent, JournalRecord, OrderType, Reservation, RuntimeOrder,
    RuntimeOrderState, Side, Submission, TimeInForce, VenueEvent,
};
use tempfile::TempDir;
use uuid::Uuid;

fn identity() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "sub-1".into(),
    }
}

fn intent_record(id: &str) -> JournalRecord {
    JournalRecord::Intent {
        submission: Submission { client_order_id: id.into(), sequence: 1 },
        intent: Intent {
            instrument: "BTCUSDT".into(),
            side: Side::Buy,
            quantity: 1.0,
            order_type: OrderType::Limit,
            limit_price: Some(100.0),
            max_quote_spend: None,
            time_in_force: TimeInForce::Gtc,
        },
        reservation: Reservation { asset: "USDT".into(), amount: 100.0 },
    }
}

fn order_event(id: &str, state: RuntimeOrderState) -> VenueEvent {
    VenueEvent::Order(RuntimeOrder {
        client_order_id: id.into(),
        venue_order_id: format!("v-{id}"),
        symbol: "BTCUSDT".into(),
        side: Side::Buy,
        original_quantity: 1.0,
        executed_quantity: if state == RuntimeOrderState::Filled { 1.0 } else { 0.0 },
        leaves_quantity: if state == RuntimeOrderState::Filled { 0.0 } else { 1.0 },
        limit_price: Some(100.0),
        state,
        original_time: Utc.timestamp_opt(0, 0).unwrap(),
    })
}

fn journal(dir: &TempDir, identity: &AccountIdentity) -> (Journal, std::path::PathBuf) {
    let path = dir.path().join("journal");
    (Journal::open(&path, identity).expect("journal"), path)
}

fn frame_end(bytes: &[u8], offset: usize) -> usize {
    let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    offset + 8 + len
}

fn patch_byte(path: &Path, offset: usize, value: u8) {
    let mut bytes = fs::read(path).unwrap();
    bytes[offset] ^= value;
    fs::write(path, bytes).unwrap();
}

#[test]
fn a_torn_final_append_is_recovered() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    let first = journal.append_durable(&intent_record("a")).unwrap();
    journal.append_durable(&intent_record("b")).unwrap();
    drop(journal);

    let len = fs::metadata(&path).unwrap().len();
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - 3).unwrap();
    drop(file);

    let mut journal = Journal::open(&path, &id).unwrap();
    assert_eq!(journal.replay().unwrap().len(), 1);
    assert_eq!(journal.last_sequence(), first);
    assert_eq!(journal.append_durable(&intent_record("c")).unwrap(), first + 1);
}

#[test]
fn a_checksum_failure_in_the_middle_blocks_admission() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    journal.append_durable(&intent_record("b")).unwrap();
    drop(journal);

    let bytes = fs::read(&path).unwrap();
    let header_end = frame_end(&bytes, 8);
    patch_byte(&path, header_end + 8, 0xff);

    assert!(matches!(Journal::open(&path, &id), Err(colo_runtime::RuntimeError::CorruptState(_))));
}

#[test]
fn a_journal_cannot_be_opened_for_another_account() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    drop(journal);

    let other = identity();
    assert!(matches!(
        Journal::open(&path, &other),
        Err(colo_runtime::RuntimeError::CorruptState(_))
    ));
}

#[test]
fn an_intent_before_send_survives_restart() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    let sequence = journal.append_durable(&intent_record("a")).unwrap();
    drop(journal);

    let journal = Journal::open(&path, &id).unwrap();
    let replay = journal.replay().unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].sequence, sequence);
    assert!(matches!(replay[0].record, JournalRecord::Intent { .. }));
    assert_eq!(journal.unacked(1, 10).unwrap().len(), 1);
}

#[test]
fn a_send_without_a_response_stays_unacknowledged() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    drop(journal);

    let journal = Journal::open(&path, &id).unwrap();
    let pending = journal.unacked(1, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].record, JournalRecord::Intent { .. }));
    assert_eq!(journal.acked(), 0);
}

#[test]
fn an_ack_is_durable_and_replayable() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    journal.append_durable(&intent_record("b")).unwrap();
    journal.ack_through(2).unwrap();
    assert!(journal.unacked(1, 10).unwrap().is_empty());
    drop(journal);

    let mut journal = Journal::open(&path, &id).unwrap();
    assert_eq!(journal.acked(), 2);
    assert!(journal.unacked(1, 10).unwrap().is_empty());
    journal.append_durable(&intent_record("c")).unwrap();
    let unacked = journal.unacked(1, 10).unwrap();
    assert_eq!(unacked.len(), 1);
    assert_eq!(unacked[0].record, intent_record("c"));
}

#[test]
fn an_ack_beyond_stored_events_is_refused() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, _) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    assert!(matches!(
        journal.ack_through(99),
        Err(colo_runtime::RuntimeError::CorruptState(_))
    ));
}

#[test]
fn an_unsupported_schema_is_refused() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("a")).unwrap();
    drop(journal);

    let mut bytes = fs::read(&path).unwrap();
    let header_start = 8;
    let payload_start = header_start + 8;
    let len = u32::from_le_bytes(bytes[header_start..header_start + 4].try_into().unwrap()) as usize;
    let payload_end = payload_start + len;
    let text = String::from_utf8(bytes[payload_start..payload_end].to_vec()).unwrap();
    let changed = text.replacen("\"schema_version\":1", "\"schema_version\":2", 1);
    assert_ne!(changed, text);
    bytes[payload_start..payload_end].copy_from_slice(changed.as_bytes());
    let crc = crc32fast::hash(&bytes[payload_start..payload_end]);
    bytes[header_start + 4..header_start + 8].copy_from_slice(&crc.to_le_bytes());
    fs::write(&path, bytes).unwrap();

    assert!(matches!(Journal::open(&path, &id), Err(colo_runtime::RuntimeError::CorruptState(_))));
}

#[test]
fn a_write_failure_returns_no_durable_sequence() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.fail_next_write_for_test();
    assert!(matches!(
        journal.append_durable(&intent_record("a")),
        Err(colo_runtime::RuntimeError::JournalIo(_))
    ));
    assert!(journal.replay().unwrap().is_empty());
    assert_eq!(journal.last_sequence(), 0);
    drop(journal);

    let journal = Journal::open(&path, &id).unwrap();
    assert!(journal.replay().unwrap().is_empty());
    assert_eq!(journal.unacked(1, 10).unwrap().len(), 0);
}

fn checkpoint_record(id: &AccountIdentity) -> JournalRecord {
    let book = colo_runtime::Book::new(id.clone(), Default::default()).unwrap();
    JournalRecord::Checkpoint { book: book.checkpoint(), cursor: Default::default() }
}

#[test]
fn compaction_without_a_checkpoint_is_refused() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, _) = journal(&dir, &id);
    journal.append_durable(&intent_record("pending")).unwrap();
    journal.ack_through(1).unwrap();
    assert!(journal.compact().is_err(), "positions would be lost");
    assert!(journal.replay().unwrap().iter().any(|r| matches!(r.record, JournalRecord::Intent { .. })));
}

#[test]
fn compaction_drops_what_the_checkpoint_covers_and_keeps_the_kill_latch() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, path) = journal(&dir, &id);
    journal.append_durable(&intent_record("done")).unwrap();
    journal
        .append_durable(&JournalRecord::Venue { event: order_event("done", RuntimeOrderState::Filled) })
        .unwrap();
    journal.append_durable(&intent_record("live")).unwrap();
    journal
        .append_durable(&JournalRecord::Kill { epoch: 7, command_id: "kill-7".into() })
        .unwrap();
    journal
        .append_durable(&JournalRecord::Policy { policy_id: "p".into(), revision: 3, epoch: 7 })
        .unwrap();
    journal.append_durable(&checkpoint_record(&id)).unwrap();
    let last = journal.last_sequence();
    journal.ack_through(last).unwrap();
    journal.compact().unwrap();

    let replay = journal.replay().unwrap();
    assert!(!replay.iter().any(|r| matches!(r.record, JournalRecord::Intent { .. })), "orders live in the checkpoint");
    assert!(replay.iter().any(|r| matches!(r.record, JournalRecord::Checkpoint { .. })));
    assert!(replay.iter().any(|r| matches!(r.record, JournalRecord::Kill { .. })));
    assert!(replay.iter().any(|r| matches!(r.record, JournalRecord::Policy { .. })));
    assert!(journal.kill_latched());
    assert_eq!(journal.last_sequence(), last + 1);
    drop(journal);

    let mut journal = Journal::open(&path, &id).unwrap();
    assert!(journal.kill_latched());
    assert!(journal.unacked(1, 10).unwrap().is_empty());
    assert_eq!(journal.append_durable(&intent_record("next")).unwrap(), last + 2);
}

#[test]
fn compaction_keeps_records_after_the_checkpoint_and_unacknowledged_ones() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut journal, _) = journal(&dir, &id);
    journal.append_durable(&intent_record("unacked")).unwrap();
    journal.append_durable(&checkpoint_record(&id)).unwrap();
    journal.append_durable(&intent_record("after")).unwrap();
    journal.compact().unwrap();
    let replay = journal.replay().unwrap();
    let kept = |name: &str| {
        replay.iter().any(|r| matches!(&r.record, JournalRecord::Intent { submission, .. } if submission.client_order_id == name))
    };
    assert!(kept("unacked"), "the OMS has not acknowledged it yet");
    assert!(kept("after"));
    assert!(journal.unacked(1, 10).unwrap().iter().all(|r| !matches!(r.record, JournalRecord::Checkpoint { .. })),
        "a checkpoint is local state, not an event for the OMS");
}

#[test]
fn a_journal_keeps_its_own_id_through_reopen_and_compaction() {
    let dir = TempDir::new().unwrap();
    let id = identity();
    let (mut first, path) = journal(&dir, &id);
    let own = first.id();
    assert!(!own.is_nil());
    first.append_durable(&checkpoint_record(&id)).unwrap();
    first.compact().unwrap();
    assert_eq!(first.id(), own);
    drop(first);
    assert_eq!(Journal::open(&path, &id).unwrap().id(), own);

    let other = TempDir::new().unwrap();
    let (fresh, _) = journal(&other, &id);
    assert_ne!(fresh.id(), own, "a fresh journal never shares an id");
}
