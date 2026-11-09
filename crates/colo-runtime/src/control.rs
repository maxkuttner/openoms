use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use colo_link::v2::{ControlAckStage, ToRuntime};
use colo_link::v2::Received;

use crate::RuntimeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Disconnected,
    Bootstrapping,
    Recovering,
    Ready,
    Suspended,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ControlAction {
    BootstrapInstalled,
    PolicyInstalled { revision: u64 },
    AuthorityGranted { deadline: Instant },
    CancelWorking { instruments: Vec<String> },
    KillLatched { epoch: u64 },
    KillCleared { stopped_epoch: u64 },
    Acked { through: u64 },
    ResyncFrom { sequence: u64 },
    Acknowledged { stage: ControlAckStage },
}

/// Persistent kill latch is deliberately separate from transient connectivity:
/// an Active policy or a reconnect cannot clear it; only an Enable naming the
/// stopped epoch can.
pub struct ControlState {
    state: RuntimeState,
    epoch: u64,
    kill_latched: Option<u64>,
    kill_command: Option<String>,
    policy_revision: u64,
    limits: BTreeMap<String, risk::Limits>,
    heartbeat_secs: u64,
    authority_deadline: Option<Instant>,
    last_authority_receipt: Option<Instant>,
}

impl ControlState {
    pub fn new(epoch: u64) -> Self {
        Self {
            state: RuntimeState::Disconnected,
            epoch,
            kill_latched: None,
            kill_command: None,
            policy_revision: 0,
            limits: BTreeMap::new(),
            heartbeat_secs: 1,
            authority_deadline: None,
            last_authority_receipt: None,
        }
    }

    pub fn state(&self) -> RuntimeState {
        self.state
    }

    pub fn begin_bootstrap(&mut self) {
        self.state = RuntimeState::Bootstrapping;
    }

    pub fn begin_recovery(&mut self) {
        self.state = RuntimeState::Recovering;
    }

    pub fn mark_ready(&mut self) {
        self.state = RuntimeState::Ready;
    }

    pub fn suspend(&mut self, _reason: &str) {
        self.state = RuntimeState::Suspended;
    }

    pub fn kill_latched(&self) -> Option<u64> {
        self.kill_latched
    }

    pub fn policy_revision(&self) -> u64 {
        self.policy_revision
    }

    pub fn authority_deadline(&self) -> Option<Instant> {
        self.authority_deadline
    }

    /// Cancellation and recovery remain possible while suspended or killed.
    pub fn cancellation_allowed(&self) -> bool {
        true
    }

    pub fn apply(&mut self, message: Received<ToRuntime>) -> Result<Vec<ControlAction>, RuntimeError> {
        let now = message.received_at;
        let mut actions = Vec::new();
        match message.message {
            ToRuntime::Bootstrap { bootstrap } => {
                bootstrap
                    .validate()
                    .map_err(|e| RuntimeError::InvalidControl(e.to_string()))?;
                let revision = bootstrap.baseline.sequence;
                if revision < self.policy_revision {
                    return Err(RuntimeError::InvalidControl("bootstrap revision rolled back".into()));
                }
                if let Some(killed) = bootstrap.killed_epoch {
                    self.kill_latched = Some(killed);
                }
                self.heartbeat_secs = bootstrap.timings.heartbeat_secs.max(1);
                self.install_limits(&bootstrap.limits, &mut actions);
                self.policy_revision = revision;
                self.state = RuntimeState::Bootstrapping;
                actions.push(ControlAction::BootstrapInstalled);
            }
            ToRuntime::Policy { policy } => {
                if policy.revision < self.policy_revision {
                    return Err(RuntimeError::InvalidControl("policy revision rolled back".into()));
                }
                self.install_limits(&policy.limits, &mut actions);
                self.policy_revision = policy.revision;
                actions.push(ControlAction::PolicyInstalled { revision: policy.revision });
            }
            ToRuntime::Authority { authority } => {
                if authority.valid_for_secs < self.heartbeat_secs {
                    return Err(RuntimeError::InvalidControl(
                        "authority is shorter than the heartbeat".into(),
                    ));
                }
                if self
                    .last_authority_receipt
                    .is_some_and(|previous| now < previous)
                {
                    return Err(RuntimeError::StaleAuthority);
                }
                let deadline = now
                    .checked_add(Duration::from_secs(authority.valid_for_secs))
                    .ok_or(RuntimeError::StaleAuthority)?;
                self.last_authority_receipt = Some(now);
                self.authority_deadline = Some(deadline);
                actions.push(ControlAction::AuthorityGranted { deadline });
            }
            ToRuntime::Kill { kill } => {
                self.kill_latched = Some(self.epoch);
                self.kill_command = Some(kill.command_id);
                self.authority_deadline = None;
                self.state = RuntimeState::Suspended;
                actions.push(ControlAction::KillLatched { epoch: self.epoch });
            }
            ToRuntime::Enable { enable } => {
                let Some(latched) = self.kill_latched else {
                    return Err(RuntimeError::InvalidControl("enable while not killed".into()));
                };
                if latched != enable.stopped_epoch {
                    return Err(RuntimeError::InvalidControl(format!(
                        "enable names epoch {}, latch is {latched}",
                        enable.stopped_epoch
                    )));
                }
                self.kill_latched = None;
                self.kill_command = None;
                self.authority_deadline = None;
                self.state = RuntimeState::Recovering;
                actions.push(ControlAction::KillCleared { stopped_epoch: latched });
            }
            ToRuntime::Rebaseline { .. } => {}
            ToRuntime::EventAck { ack } => {
                actions.push(ControlAction::Acked { through: ack.through_sequence });
            }
            ToRuntime::ResyncRequired { resync } => {
                self.state = RuntimeState::Recovering;
                actions.push(ControlAction::ResyncFrom { sequence: resync.from_sequence });
            }
        }
        Ok(actions)
    }

    fn install_limits(&mut self, limits: &BTreeMap<String, risk::Limits>, actions: &mut Vec<ControlAction>) {
        let mut reduced: Vec<String> = self
            .limits
            .keys()
            .filter(|symbol| !limits.contains_key(*symbol))
            .cloned()
            .collect();
        for (symbol, next) in limits {
            if let Some(previous) = self.limits.get(symbol) {
                if is_reduction(previous, next) {
                    reduced.push(symbol.clone());
                }
            }
        }
        if !reduced.is_empty() {
            actions.push(ControlAction::CancelWorking { instruments: reduced });
        }
        self.limits = limits.clone();
    }

    /// Admission gate. Cancellation is not gated by this method.
    pub fn may_submit(&self, now: Instant) -> Result<(), RuntimeError> {
        if self.kill_latched.is_some() {
            return Err(RuntimeError::KillLatched);
        }
        if self.state != RuntimeState::Ready {
            return Err(RuntimeError::NotReady(format!("{:?}", self.state)));
        }
        match self.authority_deadline {
            Some(deadline) if now <= deadline => Ok(()),
            Some(_) => Err(RuntimeError::StaleAuthority),
            None => Err(RuntimeError::StaleAuthority),
        }
    }
}

fn is_reduction(previous: &risk::Limits, next: &risk::Limits) -> bool {
    let fields: [(Option<f64>, Option<f64>); 5] = [
        (previous.max_order_qty, next.max_order_qty),
        (previous.max_order_notional, next.max_order_notional),
        (previous.max_position_qty, next.max_position_qty),
        (previous.max_position_notional, next.max_position_notional),
        (previous.price_band_pct, next.price_band_pct),
    ];
    let numeric = fields
        .into_iter()
        .any(|(old, new)| match (old, new) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) => new < old,
            _ => false,
        });
    let rate = match (previous.max_orders_per_sec, next.max_orders_per_sec) {
        (None, Some(_)) => true,
        (Some(old), Some(new)) => new < old,
        _ => false,
    };
    numeric || rate
}
