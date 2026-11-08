use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use colo_link::v2::{
    Allocation, Authority, Bootstrap, Enable, EventBatch, EventInbox, EventRecord, FromRuntime,
    Kill, Policy, RuntimeCheckpoint, Timings, ToRuntime, ToRuntimeEnvelope, V2Session,
    MAX_EVENT_RECORDS,
};
use gateway::runtime::{
    AccountIdentity, ExecutionCursor, InstrumentRules, Operation, RuntimeEvent, RuntimeOrder,
    RuntimeOrderState, Side, StreamState, VenueCapabilities, VenueEvent,
};
use uuid::Uuid;

fn account() -> AccountIdentity {
    AccountIdentity {
        account_id: Uuid::new_v4(),
        venue: "BINANCE".into(),
        environment: "LIVE".into(),
        external_account_ref: "sub-1".into(),
    }
}

fn rules() -> InstrumentRules {
    InstrumentRules {
        symbol: "BTCUSDT".into(),
        venue: "BINANCE".into(),
        venue_symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        tick_size: 0.01,
        step_size: 0.001,
        multiplier: 1.0,
        min_quantity: None,
        max_quantity: None,
        min_notional: None,
        max_notional: None,
        operations: vec![Operation::Place, Operation::Cancel],
        capabilities: VenueCapabilities {
            cancel_on_disconnect: false,
            atomic_cancel_replace: false,
            open_orders_snapshot: true,
            order_lookup: true,
            trade_history: true,
            balance_snapshot: true,
        },
    }
}

fn bootstrap(complete: bool) -> Bootstrap {
    Bootstrap {
        instruments: vec![rules()],
        limits: BTreeMap::new(),
        trading_state: risk::TradingState::Active,
        instrument_states: BTreeMap::new(),
        timings: Timings {
            heartbeat_secs: 1,
            authority_secs: 5,
            market_data_max_age_secs: 2,
        },
        baseline: RuntimeCheckpoint {
            sequence: 0,
            positions: BTreeMap::new(),
            open_client_ids: vec![],
            execution_cursor: ExecutionCursor::default(),
            complete,
        },
        asset_baseline: BTreeMap::new(),
        killed_epoch: None,
        allocation: Allocation {
            quote_asset: "USDT".into(),
            max_notional: 10_000.0,
            max_position_qty: Some(10.0),
        },
        recovery_universe: vec!["BTCUSDT".into()],
    }
}

fn envelope(
    version: u32,
    account: AccountIdentity,
    session: &str,
    sequence: u64,
    revision: u64,
    message: ToRuntime,
) -> ToRuntimeEnvelope {
    ToRuntimeEnvelope {
        protocol_version: version,
        account,
        epoch: 7,
        policy_revision: revision,
        sequence,
        session_nonce: session.into(),
        message,
    }
}

fn ready_session() -> (V2Session, AccountIdentity) {
    let account = account();
    let mut session = V2Session::new(account.clone(), 7, "n1", 1);
    let msg = envelope(
        2,
        account.clone(),
        "n1",
        1,
        3,
        ToRuntime::Bootstrap { bootstrap: bootstrap(true) },
    );
    session.receive_control(msg, Instant::now()).unwrap();
    (session, account)
}

fn health_event(account: &AccountIdentity) -> RuntimeEvent {
    RuntimeEvent::Health {
        account: account.clone(),
        state: StreamState::Live,
        observed_at: Utc.timestamp_opt(0, 0).unwrap(),
    }
}

#[test]
fn an_old_protocol_version_is_rejected() {
    let (mut session, account) = ready_session();
    let msg = envelope(1, account, "n1", 2, 3, ToRuntime::Kill { kill: Kill { command_id: "k".into() } });
    assert!(matches!(
        session.receive_control(msg, Instant::now()),
        Err(colo_link::v2::ProtocolError::UnsupportedVersion(1))
    ));
}

#[test]
fn another_account_is_rejected() {
    let (mut session, _) = ready_session();
    let msg = envelope(
        2,
        account(),
        "n1",
        2,
        3,
        ToRuntime::Kill { kill: Kill { command_id: "k".into() } },
    );
    assert!(matches!(
        session.receive_control(msg, Instant::now()),
        Err(colo_link::v2::ProtocolError::WrongAccount)
    ));
}

#[test]
fn an_exact_ack_is_required() {
    let (mut session, account) = ready_session();
    session.queue_events(vec![health_event(&account), health_event(&account)]).unwrap();
    let msg = |through| {
        envelope(2, account.clone(), "n1", 2, 3, ToRuntime::EventAck { ack: colo_link::v2::EventAck { through_sequence: through } })
    };
    assert!(matches!(
        session.receive_control(msg(9), Instant::now()),
        Err(colo_link::v2::ProtocolError::AckBeyondSent { .. })
    ));
    assert!(matches!(
        session.receive_control(msg(1), Instant::now()),
        Err(colo_link::v2::ProtocolError::AckNotABatchBoundary(1))
    ));
    session.receive_control(msg(2), Instant::now()).unwrap();
    assert_eq!(session.last_acked_event_sequence(), 2);
    assert!(session.pending_batches().is_empty());
}

#[test]
fn a_policy_revision_rollback_is_rejected() {
    let (mut session, account) = ready_session();
    let msg = envelope(
        2,
        account,
        "n1",
        2,
        2,
        ToRuntime::Policy { policy: Policy { limits: BTreeMap::new(), trading_state: risk::TradingState::Active, instrument_states: BTreeMap::new() } },
    );
    assert!(matches!(
        session.receive_control(msg, Instant::now()),
        Err(colo_link::v2::ProtocolError::RevisionRollback { last: 3, got: 2 })
    ));
}

#[test]
fn a_gap_in_event_sequences_is_detected_and_duplicates_are_idempotent() {
    let mut inbox = EventInbox::new(1);
    let account = account();
    let first = EventBatch {
        first_sequence: 1,
        records: vec![
            EventRecord { sequence: 1, event: health_event(&account) },
            EventRecord { sequence: 2, event: health_event(&account) },
        ],
    };
    assert_eq!(inbox.accept(&first).unwrap().len(), 2);
    assert!(inbox.accept(&first).unwrap().is_empty());
    let gap = EventBatch {
        first_sequence: 4,
        records: vec![EventRecord { sequence: 4, event: health_event(&account) }],
    };
    assert!(matches!(
        inbox.accept(&gap),
        Err(colo_link::v2::ProtocolError::SequenceGap { expected: 3, got: 4 })
    ));
}

#[test]
fn a_duplicate_bootstrap_is_idempotent() {
    let (mut session, account) = ready_session();
    let before = session.bootstrap().cloned();
    let duplicate = envelope(
        2,
        account,
        "n1",
        1,
        3,
        ToRuntime::Bootstrap { bootstrap: bootstrap(true) },
    );
    session.receive_control(duplicate, Instant::now()).unwrap();
    assert_eq!(session.bootstrap(), before.as_ref());
    assert_eq!(session.last_policy_revision(), 3);
    assert_eq!(session.last_control_sequence(), 1);
}

#[test]
fn authority_from_an_old_session_is_refused_after_reconnect() {
    let (mut session, account) = ready_session();
    session.reconnect("n2");
    let old = envelope(
        2,
        account,
        "n1",
        2,
        3,
        ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
    );
    assert!(matches!(
        session.receive_control(old, Instant::now()),
        Err(colo_link::v2::ProtocolError::SessionMismatch)
    ));
}

#[test]
fn authority_expiry_uses_receipt_time_and_heartbeats_do_not_renew_it() {
    let (mut session, account) = ready_session();
    let t0 = Instant::now();
    let authority = envelope(
        2,
        account,
        "n1",
        2,
        3,
        ToRuntime::Authority { authority: Authority { valid_for_secs: 2 } },
    );
    session.receive_control(authority, t0).unwrap();
    assert!(session.may_trade(t0));
    assert!(!session.may_trade(t0 + Duration::from_secs(3)));
}

#[test]
fn authority_shorter_than_the_heartbeat_is_rejected() {
    let (mut session, account) = ready_session();
    let msg = envelope(
        2,
        account,
        "n1",
        2,
        3,
        ToRuntime::Authority { authority: Authority { valid_for_secs: 0 } },
    );
    assert!(matches!(
        session.receive_control(msg, Instant::now()),
        Err(colo_link::v2::ProtocolError::AuthorityTooShort { .. })
    ));
}

#[test]
fn an_enable_must_name_the_stopped_epoch() {
    let (mut session, account) = ready_session();
    let kill = envelope(
        2,
        account.clone(),
        "n1",
        2,
        3,
        ToRuntime::Kill { kill: Kill { command_id: "kill".into() } },
    );
    session.receive_control(kill, Instant::now()).unwrap();
    let wrong = envelope(
        2,
        account,
        "n1",
        3,
        3,
        ToRuntime::Enable {
            enable: Enable { stopped_epoch: 6, command_id: "enable".into() },
        },
    );
    assert!(matches!(
        session.receive_control(wrong, Instant::now()),
        Err(colo_link::v2::ProtocolError::EnableEpochMismatch { stopped_epoch: 6, .. })
    ));
}

#[test]
fn event_frames_are_bounded() {
    let (mut session, account) = ready_session();
    let huge = RuntimeEvent::Venue {
        account: account.clone(),
        event: VenueEvent::Order(RuntimeOrder {
            client_order_id: "c".into(),
            venue_order_id: "v".into(),
            symbol: "X".repeat(300_000),
            side: Side::Buy,
            original_quantity: 1.0,
            executed_quantity: 0.0,
            leaves_quantity: 1.0,
            limit_price: Some(1.0),
            state: RuntimeOrderState::New,
            original_time: Utc.timestamp_opt(0, 0).unwrap(),
        }),
    };
    let batch = session.queue_events(vec![huge]).unwrap();
    let envelope = session.encode_events(&batch).unwrap();
    assert!(matches!(
        session.encode_from_runtime(&envelope),
        Err(colo_link::v2::ProtocolError::FrameTooLarge { .. })
    ));

    let too_many = vec![health_event(&account); MAX_EVENT_RECORDS + 1];
    assert!(matches!(
        session.queue_events(too_many),
        Err(colo_link::v2::ProtocolError::TooManyRecords { .. })
    ));
}

#[test]
fn pending_events_survive_reconnect_until_acked() {
    let (mut session, account) = ready_session();
    let batch = session.queue_events(vec![health_event(&account)]).unwrap();
    session.reconnect("n2");
    assert_eq!(session.pending_batches().len(), 1);
    assert_eq!(session.resend_pending(), vec![batch.clone()]);
    let ack = envelope(
        2,
        account,
        "n2",
        2,
        3,
        ToRuntime::EventAck { ack: colo_link::v2::EventAck { through_sequence: batch.last_sequence() } },
    );
    session.receive_control(ack, Instant::now()).unwrap();
    assert!(session.pending_batches().is_empty());
}

#[tokio::test]
async fn a_blocked_event_write_does_not_block_control() {
    use tokio::sync::mpsc;

    let (event_tx, _event_rx) = mpsc::channel::<FromRuntime>(1);
    let (control_tx, mut control_rx) = mpsc::channel::<FromRuntime>(1);
    event_tx
        .send(FromRuntime::Heartbeat { last_event_sequence: 1 })
        .await
        .unwrap();
    control_tx
        .send(FromRuntime::Hello { versions: vec![2], last_acked_control: 0 })
        .await
        .unwrap();
    assert!(matches!(
        control_rx.recv().await,
        Some(FromRuntime::Hello { .. })
    ));
}

#[test]
fn v1_state_cannot_deserialize_as_a_v2_bootstrap() {
    let v1 = r#"{"type":"state","active":true,"limits":{},"limits_version":1,"positions":[],"dead_man_secs":5}"#;
    assert!(serde_json::from_str::<ToRuntimeEnvelope>(v1).is_err());
}
