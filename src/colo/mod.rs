//! Co-located strategies (path ②): they trade their venue directly; the OMS
//! follows their account, books their fills, controls them over the control
//! link and can kill them without their help.

pub mod booking;
pub mod control;
pub mod follow;
pub mod link_server;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use gateway::connectors::binance::adapter::BinanceAdapter;
use colo_link::proto::{FromStrategy, ToStrategy};
use gateway::contract::{FollowEvent, FollowHandle};

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
    /// Track the open set and the order flow from one follow event.
    pub fn apply(&mut self, event: &FollowEvent, now: Instant) {
        match event {
            FollowEvent::Opened { venue_order_id, client_order_id, symbol, buy, qty, price } => {
                self.open.insert(
                    venue_order_id.clone(),
                    OpenOrder { client_order_id: client_order_id.clone(), symbol: symbol.clone(), buy: *buy, qty: *qty, price: *price },
                );
                self.flow.push_back((now, Flow::Placed));
            }
            FollowEvent::Closed { venue_order_id, outcome } => {
                self.open.remove(venue_order_id);
                match *outcome {
                    "canceled" | "expired" => self.flow.push_back((now, Flow::Cancelled)),
                    "rejected" => self.flow.push_back((now, Flow::Rejected)),
                    _ => {}
                }
            }
            FollowEvent::Fill(_) | FollowEvent::Connected => {}
        }
        while self.flow.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(60)) {
            self.flow.pop_front();
        }
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

/// One strategy's control link, as the OMS sees it.
struct LinkSlot {
    tx: mpsc::UnboundedSender<ToStrategy>,
    conn: u64,
    last_heard: Instant,
    acked_version: i64,
    kill_acked: bool,
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
    follow: FollowHandle,
    rx: Arc<Mutex<Option<mpsc::Receiver<(String, FollowEvent)>>>>,
    pub(crate) runtimes: Arc<Mutex<HashMap<String, Runtime>>>,
    adapters: Arc<Mutex<HashMap<String, Arc<BinanceAdapter>>>>,
    links: Arc<Mutex<HashMap<String, LinkSlot>>>,
    next_conn: Arc<std::sync::atomic::AtomicU64>,
    /// When each strategy's limits last changed (or its link connected).
    pub(crate) limits_changed: Arc<Mutex<HashMap<String, Instant>>>,
    /// Strategies killed while orders keep appearing: when the last one did.
    pub(crate) fighting: Arc<Mutex<HashMap<String, Instant>>>,
    pub(crate) cancel_rates: Arc<Mutex<HashMap<String, control::CancelRate>>>,
    pub(crate) alarms: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for ColoHub {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(4096);
        Self {
            follow: FollowHandle(tx),
            rx: Arc::new(Mutex::new(Some(rx))),
            runtimes: Arc::default(),
            adapters: Arc::default(),
            links: Arc::default(),
            next_conn: Arc::default(),
            limits_changed: Arc::default(),
            fighting: Arc::default(),
            cancel_rates: Arc::default(),
            alarms: Arc::default(),
        }
    }
}

impl ColoHub {
    pub fn follow_handle(&self) -> FollowHandle {
        self.follow.clone()
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
            LinkSlot { tx, conn, last_heard: Instant::now(), acked_version: 0, kill_acked: false },
        );
        self.limits_changed.lock().unwrap().insert(code.to_string(), Instant::now());
        (conn, rx)
    }

    pub(crate) fn disconnect(&self, code: &str, conn: u64) {
        let mut links = self.links.lock().unwrap();
        if links.get(code).is_some_and(|l| l.conn == conn) {
            links.remove(code);
        }
    }

    pub(crate) fn heard(&self, code: &str, conn: u64, msg: &FromStrategy) {
        let mut links = self.links.lock().unwrap();
        let Some(slot) = links.get_mut(code).filter(|l| l.conn == conn) else { return };
        slot.last_heard = Instant::now();
        match msg {
            FromStrategy::AckLimits { limits_version } => slot.acked_version = *limits_version as i64,
            FromStrategy::AckKill => slot.kill_acked = true,
            FromStrategy::Heartbeat => {}
        }
    }

    /// Send a message down a strategy's link, if it is connected.
    pub fn push(&self, code: &str, msg: ToStrategy) {
        if matches!(msg, ToStrategy::Kill) {
            if let Some(slot) = self.links.lock().unwrap().get_mut(code) {
                slot.kill_acked = false;
            }
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
                kill_acked: l.kill_acked,
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
        while let Some((code, event)) = rx.recv().await {
            self.runtimes.lock().unwrap().entry(code.clone()).or_default().apply(&event, Instant::now());
            let (Some(strategy), Some(adapter)) = (state.strategies().by_code(&code), self.adapter(&code)) else {
                warn!(strategy = %code, "co-located: event for a strategy with no follow adapter");
                continue;
            };
            control::on_event(&state, self, &strategy, &adapter, &event).await;
            match event {
                FollowEvent::Fill(fill) => match booking::book_fill(&state, &strategy, &adapter, &fill).await {
                    Ok(booking::Booked::New) => control::push_positions(&state, self, &strategy).await,
                    Ok(booking::Booked::AlreadyBooked) => {}
                    Err(e) => error!(strategy = %code, trade = %fill.trade_id, error = %e, "co-located: fill not booked; recon retries on the next reconnect"),
                },
                FollowEvent::Connected => follow::fill_gap(&state, self, &strategy, &adapter).await,
                FollowEvent::Opened { .. } | FollowEvent::Closed { .. } => {}
            }
        }
        info!("co-located: follow sink closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(id: &str) -> FollowEvent {
        FollowEvent::Opened { venue_order_id: id.into(), client_order_id: "mm1-q-1".into(), symbol: "BTCUSDT".into(), buy: true, qty: 1.0, price: 100.0 }
    }

    #[test]
    fn opened_and_closed_orders_track_the_open_set_and_counts() {
        let t0 = Instant::now();
        let mut r = Runtime::default();
        r.apply(&opened("1"), t0);
        r.apply(&opened("2"), t0);
        r.apply(&FollowEvent::Closed { venue_order_id: "1".into(), outcome: "canceled" }, t0);
        assert_eq!(r.open.len(), 1);
        assert_eq!(r.counts(t0), FlowCounts { placed: 2, cancelled: 1, rejected: 0 });
        assert_eq!(r.counts(t0 + Duration::from_secs(61)), FlowCounts::default());
    }
}
