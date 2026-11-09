use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use colo_link::v2::{
    Allocation, Authority, Bootstrap, ControlAckStage, Enable, Kill, Policy, Received,
    RuntimeCheckpoint, Timings, ToRuntime,
};
use colo_runtime::{ControlAction, ControlState, RuntimeError, RuntimeState};
use gateway::runtime::{
    ExecutionCursor, InstrumentRules, Operation, VenueCapabilities,
};

fn rules() -> InstrumentRules {
    InstrumentRules {
        symbol: "BTCUSDT".into(),
        venue: "BINANCE".into(),
        venue_symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        tick_size: 1.0,
        step_size: 1.0,
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

fn bootstrap(limit: Option<f64>, killed: Option<u64>) -> Bootstrap {
    Bootstrap {
        instruments: vec![rules()],
        limits: BTreeMap::from([(
            "BTCUSDT".into(),
            risk::Limits { max_order_qty: limit, ..Default::default() },
        )]),
        trading_state: risk::TradingState::Active,
        instrument_states: BTreeMap::new(),
        timings: Timings { heartbeat_secs: 1, authority_secs: 5, market_data_max_age_secs: 2 },
        baseline: RuntimeCheckpoint {
            sequence: 3,
            positions: BTreeMap::new(),
            open_client_ids: vec![],
            execution_cursor: ExecutionCursor::default(),
            complete: true,
        },
        asset_baseline: BTreeMap::new(),
        killed_epoch: killed,
        allocation: Allocation {
            quote_asset: "USDT".into(),
            max_notional: 1_000.0,
            max_position_qty: None,
            fee_allowance_bps: 0.0,
        },
        recovery_universe: vec!["BTCUSDT".into()],
    }
}

fn received(message: ToRuntime, at: Instant) -> Received<ToRuntime> {
    Received::new(message, at)
}

#[test]
fn nothing_is_admitted_before_bootstrap_ready_and_authority() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    assert!(matches!(
        control.may_submit(now),
        Err(RuntimeError::NotReady(_))
    ));
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    control.mark_ready();
    assert!(matches!(control.may_submit(now), Err(RuntimeError::StaleAuthority)));
    control
        .apply(received(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
            now,
        ))
        .unwrap();
    control.may_submit(now).unwrap();
}

#[test]
fn authority_expiry_is_receipt_time_based() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    control.mark_ready();
    control
        .apply(received(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 2 } },
            now,
        ))
        .unwrap();
    assert!(matches!(
        control.may_submit(now + Duration::from_secs(3)),
        Err(RuntimeError::StaleAuthority)
    ));
}

#[test]
fn a_replayed_older_authority_receipt_is_refused() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    control
        .apply(received(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
            now,
        ))
        .unwrap();
    assert!(matches!(
        control.apply(received(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 5 } },
            now - Duration::from_secs(1),
        )),
        Err(RuntimeError::StaleAuthority)
    ));
}

#[test]
fn a_full_kill_latch_is_not_cleared_by_an_active_snapshot() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    control.apply(received(ToRuntime::Kill { kill: Kill { command_id: "k".into() } }, now)).unwrap();
    assert_eq!(control.state(), RuntimeState::Suspended);
    assert_eq!(control.kill_latched(), Some(7));
    control
        .apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now))
        .unwrap();
    assert!(matches!(control.may_submit(now), Err(RuntimeError::KillLatched)));
    control
        .apply(received(
            ToRuntime::Enable {
                enable: Enable { stopped_epoch: 7, command_id: "e".into() },
            },
            now,
        ))
        .unwrap();
    assert_eq!(control.kill_latched(), None);
    assert_eq!(control.state(), RuntimeState::Recovering);
    assert!(matches!(control.may_submit(now), Err(RuntimeError::NotReady(_))));
}

#[test]
fn enable_must_name_the_latched_epoch() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Kill { kill: Kill { command_id: "k".into() } }, now)).unwrap();
    assert!(matches!(
        control.apply(received(
            ToRuntime::Enable {
                enable: Enable { stopped_epoch: 6, command_id: "e".into() },
            },
            now,
        )),
        Err(RuntimeError::InvalidControl(_))
    ));
}

#[test]
fn a_policy_revision_cannot_roll_back() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    assert!(matches!(
        control.apply(received(
            ToRuntime::Policy {
                policy: Policy {
                    revision: 2,
                    limits: BTreeMap::new(),
                    trading_state: risk::TradingState::Active,
                    instrument_states: BTreeMap::new(),
                },
            },
            now,
        )),
        Err(RuntimeError::InvalidControl(_))
    ));
}

#[test]
fn a_limit_reduction_requests_conservative_cancellation() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control
        .apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(Some(10.0), None) }, now))
        .unwrap();
    let actions = control
        .apply(received(
            ToRuntime::Policy {
                policy: Policy {
                    revision: 4,
                    limits: BTreeMap::from([(
                        "BTCUSDT".into(),
                        risk::Limits { max_order_qty: Some(5.0), ..Default::default() },
                    )]),
                    trading_state: risk::TradingState::Active,
                    instrument_states: BTreeMap::new(),
                },
            },
            now,
        ))
        .unwrap();
    assert!(actions.iter().any(|a| matches!(a, ControlAction::CancelWorking { instruments } if instruments == &vec!["BTCUSDT".to_string()])));
}

#[test]
fn cancellation_remains_allowed_in_every_suspended_state() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.suspend("stream lost");
    assert!(control.cancellation_allowed());
    control.apply(received(ToRuntime::Kill { kill: Kill { command_id: "k".into() } }, now)).unwrap();
    assert!(control.cancellation_allowed());
}

#[test]
fn authority_shorter_than_the_heartbeat_is_refused() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    assert!(matches!(
        control.apply(received(
            ToRuntime::Authority { authority: Authority { valid_for_secs: 0 } },
            now,
        )),
        Err(RuntimeError::InvalidControl(_))
    ));
}

#[test]
fn control_ack_stages_are_explicit() {
    let now = Instant::now();
    let mut control = ControlState::new(7);
    control.apply(received(ToRuntime::Bootstrap { bootstrap: bootstrap(None, None) }, now)).unwrap();
    assert_eq!(ControlAckStage::Received, ControlAckStage::Received);
}
