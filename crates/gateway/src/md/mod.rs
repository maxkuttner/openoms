//! The market data gateway: everything that brings prices in. The order side
//! of the gateway ([`crate::adapter`]) carries none.
//!
//! The core says what it wants priced ([`MdHandle::subscribe`] / [`MdHandle::unsubscribe`]);
//! the gateway decides which feed covers what, runs the sessions and emits every
//! [`Quote`] on the one channel [`MarketDataGateway::new`] hands back.

pub mod daily_stats;
pub mod opra;
pub mod session;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use dataprovider::{InstrumentRef, Quote};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::connectors::QuoteSource;
use crate::stream_health::{StreamHandle, StreamHealthRegistry, StreamKind};
use session::FeedSession;

/// What is held, by instrument id.
type Held = BTreeMap<i64, InstrumentRef>;

/// How many quotes may wait for the mark router before feeds block.
const QUOTE_CHANNEL_CAPACITY: usize = 1024;

/// The core's handle on the market data gateway: what it wants priced.
///
/// Each subscription belongs to an owner (the held set, a strategy). An
/// instrument stays subscribed while any owner wants it, so one owner letting
/// go never drops another's.
#[derive(Clone)]
pub struct MdHandle(Arc<Interests>);

struct Interests {
    /// The union the feed sessions watch.
    held: watch::Sender<Held>,
    /// instrument id → (what to subscribe, who wants it).
    owners: Mutex<BTreeMap<i64, (InstrumentRef, BTreeSet<String>)>>,
}

impl MdHandle {
    fn new() -> Self {
        let (held, _) = watch::channel(Held::new());
        Self(Arc::new(Interests { held, owners: Mutex::new(BTreeMap::new()) }))
    }

    /// `owner` wants these instruments priced (insert, or replace by id).
    pub fn subscribe(&self, owner: &str, instruments: Vec<InstrumentRef>) {
        self.update(|owners| {
            for i in instruments {
                let entry = owners.entry(i.id).or_insert_with(|| (i.clone(), BTreeSet::new()));
                entry.0 = i;
                entry.1.insert(owner.to_string());
            }
        });
    }

    /// `owner` no longer wants these. A running session keeps an instrument
    /// nobody wants until its next reconnect: feeds cannot unsubscribe mid-session.
    pub fn unsubscribe(&self, owner: &str, instrument_ids: &[i64]) {
        self.update(|owners| {
            for id in instrument_ids {
                if let Some((_, who)) = owners.get_mut(id) {
                    who.remove(owner);
                }
            }
        });
    }

    /// Drop everything `owner` wanted.
    pub fn unsubscribe_owner(&self, owner: &str) {
        self.update(|owners| {
            for (_, who) in owners.values_mut() {
                who.remove(owner);
            }
        });
    }

    fn update(&self, change: impl FnOnce(&mut BTreeMap<i64, (InstrumentRef, BTreeSet<String>)>)) {
        let mut owners = self.0.owners.lock().unwrap();
        change(&mut owners);
        owners.retain(|_, (_, who)| !who.is_empty());
        let union: Held = owners.iter().map(|(id, (i, _))| (*id, i.clone())).collect();
        self.0.held.send_if_modified(|held| {
            if *held == union {
                return false;
            }
            *held = union;
            true
        });
    }

    fn watch(&self) -> watch::Receiver<Held> {
        self.0.held.subscribe()
    }
}

pub struct MarketDataGateway {
    quote_tx: mpsc::Sender<Quote>,
    held: MdHandle,
    health: StreamHealthRegistry,
}

impl MarketDataGateway {
    /// The gateway and the one receiver of every quote it emits (the mark router's input).
    pub fn new(health: StreamHealthRegistry) -> (Self, mpsc::Receiver<Quote>) {
        let (quote_tx, quote_rx) = mpsc::channel(QUOTE_CHANNEL_CAPACITY);
        (Self { quote_tx, held: MdHandle::new(), health }, quote_rx)
    }

    pub fn handle(&self) -> MdHandle {
        self.held.clone()
    }

    /// Spawn every connector's public feed, supervised under its label, for the
    /// life of the process. Not registered anywhere: no credential can change
    /// under a public feed, so nothing ever restarts one.
    pub fn start_public_feeds(&self) {
        for connector in crate::connectors::all() {
            if let Some(feed) = connector.public_feed() {
                let health = self.health.handle(connector.manifest().code, "SPOT", StreamKind::Feed);
                self.start_vendor_feed((feed.source)(), feed.label, health);
            }
        }
    }

    /// Run one feed under `supervise(label, …)`; the caller keeps the handle to
    /// stop it (a credentialed feed whose key can change).
    pub fn start_vendor_feed(&self, source: Arc<dyn QuoteSource>, label: &'static str, health: StreamHandle) -> JoinHandle<()> {
        let session = self.session(source, health.clone());
        // `supervise` never returns; the block gives the task a `()` output.
        tokio::spawn(async move {
            crate::stream_supervisor::supervise(label, health, session).await;
        })
    }

    pub(crate) fn session(&self, source: Arc<dyn QuoteSource>, health: StreamHandle) -> FeedSession {
        FeedSession::new(source, self.held.watch(), self.quote_tx.clone(), health)
    }
}
