//! The market data gateway: everything that brings prices in. The order side
//! of the gateway ([`crate::adapter`]) carries none.
//!
//! The core says what is held ([`MdHandle::subscribe`] / [`MdHandle::unsubscribe`]);
//! the gateway decides which feed covers what, runs the sessions and emits every
//! [`Quote`] on the one channel [`MarketDataGateway::new`] hands back.

pub mod daily_stats;
pub mod session;

use std::collections::BTreeMap;
use std::sync::Arc;

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

/// The core's handle on the market data gateway: what is held.
#[derive(Clone)]
pub struct MdHandle(Arc<watch::Sender<Held>>);

impl MdHandle {
    /// Hold these instruments (insert, or replace by id).
    pub fn subscribe(&self, instruments: Vec<InstrumentRef>) {
        self.0.send_if_modified(|held| {
            let mut changed = false;
            for i in instruments {
                if held.get(&i.id) != Some(&i) {
                    held.insert(i.id, i);
                    changed = true;
                }
            }
            changed
        });
    }

    /// Stop holding these. A running session keeps them until its next reconnect:
    /// feeds cannot unsubscribe mid-session.
    pub fn unsubscribe(&self, instrument_ids: &[i64]) {
        self.0.send_if_modified(|held| {
            let before = held.len();
            for id in instrument_ids {
                held.remove(id);
            }
            held.len() != before
        });
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
        let (held, _) = watch::channel(Held::new());
        (Self { quote_tx, held: MdHandle(Arc::new(held)), health }, quote_rx)
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
        FeedSession::new(source, self.held.0.subscribe(), self.quote_tx.clone(), health)
    }
}
