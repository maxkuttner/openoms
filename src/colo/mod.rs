//! Co-located strategies (path ②): they trade their venue directly; the OMS
//! follows their account (its fills book through ingest, see `follow`),
//! controls them over the control link and can kill them without their help.

pub mod control;
pub mod deployment;
pub mod follow;
pub mod ingest;
pub mod link_server;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use gateway::connectors::binance::adapter::BinanceAdapter;
use colo_link::proto::{FromStrategy, ToStrategy};
use colo_link::v2::{FromRuntimeEnvelope, ToRuntime, ToRuntimeEnvelope, PROTOCOL_VERSION};
use gateway::venue_event::{VenueEvent, VenueOrderStatus};

use crate::app_state::AppState;

/// An order open on a followed account, held in memory only.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    pub client_order_id: String,
    pub symbol: String,
    pub buy: bool,
    pub qty: f64,
    pub price: f64,
}

/// What one venue event on a followed account means to the co-located
/// runtime: an order appeared, one closed, or a fill.
#[derive(Debug, Clone, PartialEq)]
pub enum AccountEvent {
    Opened { venue_order_id: String, symbol: String },
    /// `"canceled"`, `"rejected"`, `"expired"` or `"filled"`.
    Closed { venue_order_id: String, outcome: &'static str },
    Filled,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
    Placed,
    Cancelled,
    Rejected,
}

/// What the OMS knows about one co-located strategy right now.
#[derive(Default)]
pub struct Runtime {
    pub open: HashMap<String, OpenOrder>,
    flow: VecDeque<(Instant, Flow)>,
}

/// Placed / cancelled / rejected orders in the last minute.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct FlowCounts {
    pub placed: usize,
    pub cancelled: usize,
    pub rejected: usize,
}

impl Runtime {
    /// Track the open set and the order flow from one venue event on the
    /// account; what it meant, if anything. An order is placed the first
    /// time it is seen working.
    pub fn apply(&mut self, event: &VenueEvent, now: Instant) -> Option<AccountEvent> {
        let meant = match event {
            VenueEvent::Order(o) => {
                let outcome = match &o.status {
                    VenueOrderStatus::Working => None,
                    VenueOrderStatus::Filled => Some("filled"),
                    VenueOrderStatus::Canceled => Some("canceled"),
                    VenueOrderStatus::Expired => Some("expired"),
                    VenueOrderStatus::Rejected { .. } => Some("rejected"),
                };
                match outcome {
                    None if self.open.contains_key(&o.venue_order_id) => None,
                    None => {
                        self.open.insert(
                            o.venue_order_id.clone(),
                            OpenOrder {
                                client_order_id: o.client_order_id.clone().unwrap_or_default(),
                                symbol: o.symbol.clone(),
                                buy: o.buy,
                                qty: o.qty,
                                price: o.limit_px.unwrap_or(0.0),
                            },
                        );
                        self.flow.push_back((now, Flow::Placed));
                        Some(AccountEvent::Opened { venue_order_id: o.venue_order_id.clone(), symbol: o.symbol.clone() })
                    }
                    Some(outcome) => {
                        self.open.remove(&o.venue_order_id);
                        match outcome {
                            "canceled" | "expired" => self.flow.push_back((now, Flow::Cancelled)),
                            "rejected" => self.flow.push_back((now, Flow::Rejected)),
                            _ => {}
                        }
                        Some(AccountEvent::Closed { venue_order_id: o.venue_order_id.clone(), outcome })
                    }
                }
            }
            VenueEvent::Fill(_) => Some(AccountEvent::Filled),
            VenueEvent::Fee { .. } | VenueEvent::Connected | VenueEvent::Disconnected => None,
        };
        while self.flow.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(60)) {
            self.flow.pop_front();
        }
        meant
    }

    /// Orders placed within `window` of now.
    pub fn placed_within(&self, window: Duration) -> usize {
        let now = Instant::now();
        self.flow.iter().filter(|(t, f)| *f == Flow::Placed && now.duration_since(*t) <= window).count()
    }

    pub fn counts(&self, now: Instant) -> FlowCounts {
        let mut c = FlowCounts::default();
        for (t, f) in &self.flow {
            if now.duration_since(*t) <= Duration::from_secs(60) {
                match f {
                    Flow::Placed => c.placed += 1,
                    Flow::Cancelled => c.cancelled += 1,
                    Flow::Rejected => c.rejected += 1,
                }
            }
        }
        c
    }
}

/// One worker task per strategy: its events stay in order, and a slow
/// strategy (a long gap fill, a slow venue) never holds up another's.
pub struct Workers<T>(HashMap<String, mpsc::UnboundedSender<T>>);

impl<T> Default for Workers<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<T> Workers<T> {
    /// Hand `item` to `code`'s worker, starting one (again) if it has none.
    pub fn send(&mut self, code: &str, item: T, start: impl FnOnce(&str, mpsc::UnboundedReceiver<T>)) {
        let item = match self.0.get(code) {
            Some(tx) => match tx.send(item) {
                Ok(()) => return,
                Err(mpsc::error::SendError(item)) => item,
            },
            None => item,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(item);
        self.0.insert(code.to_string(), tx);
        start(code, rx);
    }
}

/// One strategy's control link, as the OMS sees it.
struct LinkSlot {
    tx: mpsc::UnboundedSender<ToStrategy>,
    conn: u64,
    last_heard: Instant,
    acked_version: i64,
}

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct LinkStatus {
    pub connected: bool,
    pub last_heard_secs: Option<u64>,
    pub acked_version: i64,
    pub kill_acked: bool,
}

/// Every co-located strategy's runtime state, and the sink their follow
/// streams report to. Cheap to clone.
#[derive(Clone)]
pub struct ColoHub {
    follow: mpsc::Sender<(String, VenueEvent)>,
    rx: Arc<Mutex<Option<mpsc::Receiver<(String, VenueEvent)>>>>,
    pub(crate) runtimes: Arc<Mutex<HashMap<String, Runtime>>>,
    pub(crate) adapters: Arc<Mutex<HashMap<String, Arc<BinanceAdapter>>>>,
    /// Strategies whose account is followed, with their follow key's fingerprint.
    pub(crate) followed: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
    links: Arc<Mutex<HashMap<String, LinkSlot>>>,
    next_conn: Arc<std::sync::atomic::AtomicU64>,
    /// When each strategy's limits last changed (or its link connected).
    pub(crate) limits_changed: Arc<Mutex<HashMap<String, Instant>>>,
    /// Strategies killed while orders keep appearing: when the last one did.
    pub(crate) fighting: Arc<Mutex<HashMap<String, Instant>>>,
    pub(crate) cancel_rates: Arc<Mutex<HashMap<String, control::CancelRate>>>,
    pub(crate) alarms: Arc<Mutex<HashMap<String, String>>>,
    /// The last kill sent to each strategy, and whether it acknowledged it.
    kills: Arc<Mutex<HashMap<String, (Instant, bool)>>>,
    /// When each strategy's link went down; strategies never connected count
    /// from `started`.
    down_since: Arc<Mutex<HashMap<String, Instant>>>,
    started: Instant,
    v2_control: Arc<Mutex<HashMap<Uuid, V2ControlSlot>>>,
    /// One inbox drain at a time per deployment, so the background drain and
    /// the drain after a new batch never book the same fill twice.
    drains: Arc<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
}

struct V2ControlSlot {
    conn: u64,
    strategy_code: String,
    tx: mpsc::UnboundedSender<ToRuntimeEnvelope>,
    account: gateway::runtime::AccountIdentity,
    epoch: u64,
    policy_revision: u64,
    session_nonce: String,
    next_sequence: u64,
}

impl Default for ColoHub {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(4096);
        Self {
            follow: tx,
            rx: Arc::new(Mutex::new(Some(rx))),
            runtimes: Arc::default(),
            adapters: Arc::default(),
            followed: Arc::default(),
            links: Arc::default(),
            next_conn: Arc::default(),
            limits_changed: Arc::default(),
            fighting: Arc::default(),
            cancel_rates: Arc::default(),
            alarms: Arc::default(),
            kills: Arc::default(),
            down_since: Arc::default(),
            started: Instant::now(),
            drains: Arc::new(Mutex::new(HashMap::new())),
            v2_control: Arc::default(),
        }
    }
}

impl ColoHub {
    /// What a strategy's follow stream saw on its account.
    pub(crate) async fn observe(&self, code: &str, event: VenueEvent) {
        if self.follow.send((code.to_string(), event)).await.is_err() {
            error!(strategy = code, "co-located: follow sink is gone; event dropped");
        }
    }

    /// Register the deployment's control socket. Bootstrap was already sent
    /// with sequence 1, so commands start at 2.
    pub(crate) fn register_v2_control(
        &self,
        deployment_id: Uuid,
        strategy_code: &str,
        account: gateway::runtime::AccountIdentity,
        epoch: u64,
        policy_revision: u64,
        session_nonce: String,
    ) -> (u64, mpsc::UnboundedReceiver<ToRuntimeEnvelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = self.next_conn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.v2_control.lock().unwrap().insert(
            deployment_id,
            V2ControlSlot {
                conn,
                strategy_code: strategy_code.to_string(),
                tx,
                account,
                epoch,
                policy_revision,
                session_nonce,
                next_sequence: 2,
            },
        );
        self.down_since.lock().unwrap().remove(strategy_code);
        (conn, rx)
    }

    pub(crate) fn disconnect_v2_control(&self, deployment_id: Uuid, conn: u64) {
        let mut links = self.v2_control.lock().unwrap();
        if links.get(&deployment_id).is_some_and(|slot| slot.conn == conn) {
            if let Some(slot) = links.remove(&deployment_id) {
                self.down_since.lock().unwrap().insert(slot.strategy_code, Instant::now());
            }
        }
    }

    /// Queue a durable command on the deployment's control socket, if connected.
    pub(crate) fn drain_guard(&self, deployment_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.drains.lock().unwrap().entry(deployment_id).or_default().clone()
    }

    pub(crate) fn push_v2_control(&self, deployment_id: Uuid, message: ToRuntime) -> bool {
        let mut links = self.v2_control.lock().unwrap();
        let Some(slot) = links.get_mut(&deployment_id) else { return false };
        if let ToRuntime::Policy { policy } = &message {
            slot.policy_revision = policy.revision;
        }
        let sequence = slot.next_sequence;
        slot.next_sequence = slot.next_sequence.saturating_add(1);
        let envelope = ToRuntimeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            account: slot.account.clone(),
            epoch: slot.epoch,
            policy_revision: slot.policy_revision,
            sequence,
            session_nonce: slot.session_nonce.clone(),
            message,
        };
        slot.tx.send(envelope).is_ok()
    }

    /// The OMS's follow-only adapter on a strategy's account.
    pub fn adapter(&self, code: &str) -> Option<Arc<BinanceAdapter>> {
        self.adapters.lock().unwrap().get(code).cloned()
    }

    pub(crate) fn set_adapter(&self, code: &str, adapter: Arc<BinanceAdapter>) {
        self.adapters.lock().unwrap().insert(code.to_string(), adapter);
    }

    /// A strategy's link connected: it replaces any older connection of it.
    pub(crate) fn connect(&self, code: &str) -> (u64, mpsc::UnboundedReceiver<ToStrategy>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = self.next_conn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.links.lock().unwrap().insert(
            code.to_string(),
            LinkSlot { tx, conn, last_heard: Instant::now(), acked_version: 0 },
        );
        self.down_since.lock().unwrap().remove(code);
        self.limits_changed.lock().unwrap().insert(code.to_string(), Instant::now());
        (conn, rx)
    }

    pub(crate) fn disconnect(&self, code: &str, conn: u64) {
        let mut links = self.links.lock().unwrap();
        if links.get(code).is_some_and(|l| l.conn == conn) {
            links.remove(code);
            self.down_since.lock().unwrap().insert(code.to_string(), Instant::now());
        }
    }

    /// Whether a runtime of this strategy holds a v2 control connection.
    /// Its policy acknowledgements are tracked on the deployment, not here.
    pub fn v2_connected(&self, code: &str) -> bool {
        self.v2_control.lock().unwrap().values().any(|slot| slot.strategy_code == code)
    }

    /// How long a strategy has had no link (v1 or v2); `None` while connected.
    pub fn link_down_for(&self, code: &str) -> Option<Duration> {
        if self.links.lock().unwrap().contains_key(code) || self.v2_connected(code) {
            return None;
        }
        let since = self.down_since.lock().unwrap().get(code).copied().unwrap_or(self.started);
        Some(since.elapsed())
    }

    /// When a connection last heard its strategy; `None` once it is replaced.
    pub(crate) fn last_heard(&self, code: &str, conn: u64) -> Option<Instant> {
        self.links.lock().unwrap().get(code).filter(|l| l.conn == conn).map(|l| l.last_heard)
    }

    pub fn kill_ack_late(&self, code: &str) -> bool {
        control::kill_ack_late(self.kills.lock().unwrap().get(code).copied(), Instant::now())
    }

    pub(crate) fn heard(&self, code: &str, conn: u64, msg: &FromStrategy) {
        let mut links = self.links.lock().unwrap();
        let Some(slot) = links.get_mut(code).filter(|l| l.conn == conn) else { return };
        slot.last_heard = Instant::now();
        match msg {
            FromStrategy::AckLimits { limits_version } => slot.acked_version = *limits_version as i64,
            FromStrategy::AckKill => {
                if let Some(kill) = self.kills.lock().unwrap().get_mut(code) {
                    kill.1 = true;
                }
            }
            FromStrategy::Heartbeat => {}
        }
    }

    /// Send a message down a strategy's link, if it is connected.
    pub fn push(&self, code: &str, msg: ToStrategy) {
        match msg {
            ToStrategy::Kill => {
                self.kills.lock().unwrap().insert(code.to_string(), (Instant::now(), false));
            }
            ToStrategy::Resume => {
                self.kills.lock().unwrap().remove(code);
            }
            _ => {}
        }
        if matches!(msg, ToStrategy::Limits { .. }) {
            self.limits_changed.lock().unwrap().insert(code.to_string(), Instant::now());
        }
        if let Some(slot) = self.links.lock().unwrap().get(code) {
            let _ = slot.tx.send(msg);
        }
    }

    pub fn link_status(&self, code: &str) -> LinkStatus {
        match self.links.lock().unwrap().get(code) {
            Some(l) => LinkStatus {
                connected: true,
                last_heard_secs: Some(l.last_heard.elapsed().as_secs()),
                acked_version: l.acked_version,
                kill_acked: self.kills.lock().unwrap().get(code).is_some_and(|k| k.1),
            },
            None if self.v2_connected(code) => LinkStatus {
                connected: true,
                kill_acked: self.kills.lock().unwrap().get(code).is_some_and(|k| k.1),
                ..LinkStatus::default()
            },
            None => LinkStatus::default(),
        }
    }

    pub fn alarm(&self, code: &str) -> Option<String> {
        self.alarms.lock().unwrap().get(code).cloned()
    }

    /// Killed, and the strategy kept placing orders in the last minute.
    pub fn fighting(&self, code: &str) -> bool {
        self.fighting.lock().unwrap().get(code).is_some_and(|t| t.elapsed() < Duration::from_secs(60))
    }

    pub fn open_orders(&self, code: &str) -> usize {
        self.runtimes.lock().unwrap().get(code).map_or(0, |r| r.open.len())
    }

    pub fn flow(&self, code: &str) -> FlowCounts {
        self.runtimes.lock().unwrap().get(code).map(|r| r.counts(Instant::now())).unwrap_or_default()
    }

    /// Consume what the follow streams report, for the life of the process.
    pub async fn run(&self, state: AppState) {
        let Some(mut rx) = self.rx.lock().unwrap().take() else { return };
        let mut workers = Workers::default();
        while let Some((code, event)) = rx.recv().await {
            let meant = self.runtimes.lock().unwrap().entry(code.clone()).or_default().apply(&event, Instant::now());
            workers.send(&code, (meant, event), |code, events| {
                tokio::spawn(self.clone().work(state.clone(), code.to_string(), events));
            });
        }
        info!("co-located: follow sink closed");
    }

    /// One strategy's events, in order: the kill fight and alarms, and the
    /// colo trade ledger. Booking is the follow reconciler's (ingest).
    async fn work(self, state: AppState, code: String, mut events: mpsc::UnboundedReceiver<(Option<AccountEvent>, VenueEvent)>) {
        while let Some((meant, event)) = events.recv().await {
            let (Some(strategy), Some(adapter)) = (state.strategies().by_code(&code), self.adapter(&code)) else {
                warn!(strategy = %code, "co-located: event for a strategy with no follow adapter");
                continue;
            };
            if let Some(meant) = &meant {
                control::on_event(&state, &self, &strategy, &adapter, meant).await;
            }
            if let VenueEvent::Fill(fill) = event {
                if let Ok(Some(identity)) = ingest::identity_for_strategy(state.pool(), &strategy.code).await {
                    if let Err(e) = ingest::observe_external_fill(&state, &identity, &fill).await {
                        warn!(strategy = %code, trade = %fill.exec_id, error = %e, "co-located: fill not recorded in the durable ledger");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: &str, status: VenueOrderStatus) -> VenueEvent {
        VenueEvent::Order(gateway::venue_event::VenueOrderUpdate {
            venue_order_id: id.into(),
            client_order_id: Some("mm1-q-1".into()),
            account_ref: "sub".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            order_type: "LIMIT".into(),
            qty: 1.0,
            limit_px: Some(100.0),
            status,
            cum_qty: 0.0,
            ts: None,
        })
    }

    #[test]
    fn each_strategy_gets_one_worker_in_order() {
        let mut workers = Workers::default();
        let (mut started, mut alive) = (Vec::new(), Vec::new());
        for (code, n) in [("mm1", 1), ("mm2", 2), ("mm1", 3)] {
            workers.send(code, n, |code, rx| {
                started.push(code.to_string());
                alive.push(rx);
            });
        }
        assert_eq!(started, ["mm1", "mm2"]);
    }

    #[test]
    fn a_dead_worker_is_restarted() {
        let mut workers = Workers::default();
        let mut started = 0;
        workers.send("mm1", 1, |_, rx| { drop(rx); started += 1 });
        workers.send("mm1", 2, |_, rx| { std::mem::forget(rx); started += 1 });
        assert_eq!(started, 2);
    }

    #[test]
    fn opened_and_closed_orders_track_the_open_set_and_counts() {
        let t0 = Instant::now();
        let mut r = Runtime::default();
        assert_eq!(
            r.apply(&order("1", VenueOrderStatus::Working), t0),
            Some(AccountEvent::Opened { venue_order_id: "1".into(), symbol: "BTCUSDT".into() })
        );
        r.apply(&order("2", VenueOrderStatus::Working), t0);
        assert_eq!(r.apply(&order("2", VenueOrderStatus::Working), t0), None, "a partial fill is not a new order");
        assert_eq!(
            r.apply(&order("1", VenueOrderStatus::Canceled), t0),
            Some(AccountEvent::Closed { venue_order_id: "1".into(), outcome: "canceled" })
        );
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.counts(t0), FlowCounts { placed: 2, cancelled: 1, rejected: 0 });
        assert_eq!(r.counts(t0 + Duration::from_secs(61)), FlowCounts::default());
    }
}
