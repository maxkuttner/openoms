use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use arc_swap::ArcSwap;
use sqlx::PgPool;
use symbology::{Identifier, InMemoryCache, OpenFigiClient};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::adapters::BrokerRegistry;
use crate::kafka::KafkaClient;
use crate::marks::MarkStore;
use crate::stream_health::StreamHealthRegistry;

/// The OpenFIGI-backed instrument identification engine, with an in-memory cache.
pub type SymbologyEngine = Identifier<OpenFigiClient, InMemoryCache>;

/// Handles for the supervised feed tasks, keyed by connection code, so a
/// credential change can stop one and start its replacement.
///
/// A `Mutex<HashMap<..>>`, unlike the broker registry's `ArcSwap`: feeds are
/// not on the order path, so a lock here costs nothing, and what this needs is
/// point `insert`/`remove` on individual entries — `ArcSwap` only publishes
/// whole-snapshot replacements, which is the wrong shape for "stop and restart
/// just this one feed" without disturbing the others.
///
/// `abort()` rather than a graceful stop is acceptable *for feeds specifically*:
/// `stream_supervisor` already treats a dropped stream as a disconnect to
/// reconnect from, so losing an in-flight quote to an abort is indistinguishable
/// from losing it to a network blip — which the system already tolerates by
/// design. That reasoning does not extend to broker sessions, which is exactly
/// why FIX is out of scope for this plan.
#[derive(Clone, Default)]
pub struct StreamRegistry {
    handles: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly spawned feed task under `code`. Does not stop
    /// anything already registered there — callers replacing a running feed
    /// must call `abort_and_remove` first (see `reload.rs`), or the old task
    /// keeps running unsupervised, double-subscribed alongside the new one.
    pub fn insert(&self, code: impl Into<String>, handle: JoinHandle<()>) {
        self.handles.lock().unwrap().insert(code.into(), handle);
    }

    /// Stop, join and forget the task registered under `code`, if any. Returns
    /// whether a task was removed. A no-op when
    /// nothing is registered there — e.g. first boot, or a feed that never
    /// started.
    pub async fn abort_and_remove(&self, code: &str) -> bool {
        // Drop the map lock before waiting. abort() only schedules cancellation;
        // joining proves the old task has released its socket before replacement.
        let handle = self.handles.lock().unwrap().remove(code);
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
            true
        } else {
            false
        }
    }
}

/// Fan-out targets for the "a fill changed positions, go re-check what's held"
/// doorbell, keyed by feed code.
///
/// Keyed, rather than a plain `Vec`, so that restarting a feed *replaces* its
/// entry instead of appending a second one: the old sender's receiver is owned
/// by the task `StreamRegistry::abort_and_remove` just aborted, so an
/// unreplaced old entry would sit forever pointing at a dead channel, and
/// every future ring would keep `try_send`ing into it for no reason.
///
/// `std::sync::Mutex`, not an `ArcSwap`: the critical section here is the
/// `try_send` loop in `ring_all`, which never awaits, and it only ever runs on
/// the fill path, which is not high-frequency — the same reasoning as
/// `StreamRegistry`'s mutex, and the opposite of the broker registry's
/// `ArcSwap`, which sits on the much hotter, latency-sensitive order path.
#[derive(Clone, Default)]
pub struct DoorbellRegistry {
    senders: Arc<Mutex<HashMap<String, mpsc::Sender<()>>>>,
}

impl DoorbellRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tx` as the doorbell target for `code`, replacing whatever was
    /// registered there before (see the struct doc comment for why replacing,
    /// not accumulating, matters).
    pub fn set(&self, code: impl Into<String>, tx: mpsc::Sender<()>) {
        self.senders.lock().unwrap().insert(code.into(), tx);
    }

    pub fn remove(&self, code: &str) {
        self.senders.lock().unwrap().remove(code);
    }

    /// Wake every registered feed. Non-blocking: a full per-feed channel
    /// already means "reload pending" for that feed, so a failed `try_send`
    /// is fine to ignore.
    pub fn ring_all(&self) {
        for tx in self.senders.lock().unwrap().values() {
            let _ = tx.try_send(());
        }
    }
}

/// Shared application state injected into every Axum handler via State<AppState>.
#[derive(Clone)]
pub struct AppState {
    pool: PgPool,
    pub admin_token: String,
    pub admin_auth_enabled: bool,
    /// Swappable so a credential change can publish a new registry without
    /// restarting: readers on the order path take an atomic load, and an order
    /// already routing holds its own `Arc` and finishes against the adapter it
    /// started with. A `Mutex` here would put a lock on every order.
    registry: Arc<ArcSwap<BrokerRegistry>>,
    /// Serialize the entire reload, including store reads and stream joins.
    /// Order readers still use ArcSwap and never acquire this lock.
    reload_lock: Arc<tokio::sync::Mutex<()>>,
    kafka: Option<KafkaClient>,
    symbology: Arc<SymbologyEngine>,
    stream_health: StreamHealthRegistry,
    marks: MarkStore,
    /// Handles for Databento and Alpaca/Binance REST execution tasks. Public
    /// Binance/Bybit market-data feeds are never registered here.
    streams: StreamRegistry,
    /// Fan-out targets for the fill→marks doorbell. Owned here — not just a
    /// `serve()` local — so the reload handler can register a restarted feed's
    /// replacement sender into the exact map the boot-time fan-out task already
    /// reads from; see `DoorbellRegistry`'s own doc comment for why keyed
    /// replacement, not accumulation, matters.
    doorbells: DoorbellRegistry,
    /// Sender half of the fill→marks doorbell, so a reload can hand a restarted
    /// FIX/Alpaca adapter the *same* channel boot wired up rather than a copy
    /// nothing reads. `None` only in a test fixture that never signs up for the
    /// doorbell at all.
    position_changed_tx: Option<mpsc::Sender<()>>,
    /// Where every market-data feed publishes quotes. Stored so a reload can
    /// hand a restarted feed the same channel the boot-time `mark_router` task
    /// already reads from, instead of constructing a second, disconnected one.
    quote_tx: mpsc::Sender<dataprovider::Quote>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        admin_token: String,
        admin_auth_enabled: bool,
        registry: BrokerRegistry,
        kafka: Option<KafkaClient>,
        symbology: SymbologyEngine,
        stream_health: StreamHealthRegistry,
        position_changed_tx: Option<mpsc::Sender<()>>,
        quote_tx: mpsc::Sender<dataprovider::Quote>,
    ) -> Self {
        Self {
            pool,
            admin_token,
            admin_auth_enabled,
            registry: Arc::new(ArcSwap::from_pointee(registry)),
            reload_lock: Arc::new(tokio::sync::Mutex::new(())),
            kafka,
            symbology: Arc::new(symbology),
            stream_health,
            marks: MarkStore::new(),
            streams: StreamRegistry::new(),
            doorbells: DoorbellRegistry::new(),
            position_changed_tx,
            quote_tx,
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The live registry. The returned guard derefs to `BrokerRegistry`, so
    /// existing call sites are unchanged; hold it only as long as needed.
    pub fn registry(&self) -> arc_swap::Guard<Arc<BrokerRegistry>> {
        self.registry.load()
    }

    /// Publish a new registry. Readers see it on their next `registry()` call;
    /// anything already holding an adapter is unaffected. Called by the
    /// `/admin/connections/reload` handler in `admin.rs`, and by boot itself
    /// (see `serve()`) so the very first publish exercises the same path.
    pub fn swap_registry(&self, next: BrokerRegistry) {
        self.registry.store(Arc::new(next));
    }

    pub async fn lock_reload(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reload_lock.lock().await
    }

    pub fn kafka(&self) -> Option<&KafkaClient> {
        self.kafka.as_ref()
    }

    pub fn symbology(&self) -> &Arc<SymbologyEngine> {
        &self.symbology
    }

    pub fn stream_health(&self) -> &StreamHealthRegistry {
        &self.stream_health
    }

    pub fn marks(&self) -> &MarkStore { &self.marks }

    pub fn streams(&self) -> &StreamRegistry {
        &self.streams
    }

    pub fn doorbells(&self) -> &DoorbellRegistry {
        &self.doorbells
    }

    /// A fresh handle on the fill→marks doorbell sender, for handing to a
    /// freshly built `RegistrationDeps` (see `reload.rs`). Cloning a
    /// `mpsc::Sender` is cheap — it is a handle onto the shared channel state,
    /// not the channel itself.
    pub fn position_changed_tx(&self) -> Option<mpsc::Sender<()>> {
        self.position_changed_tx.clone()
    }

    /// A fresh handle on the shared quote channel every market-data feed
    /// publishes onto, for restarting a feed outside of boot (see
    /// `reload::restart_databento_feed`).
    pub fn quote_tx(&self) -> mpsc::Sender<dataprovider::Quote> {
        self.quote_tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::alpaca::AlpacaAdapter;

    /// `PgPool::connect_lazy` builds a pool without ever connecting, so `AppState`
    /// can be constructed in a unit test without a real database.
    fn sample_state() -> AppState {
        let pool = PgPool::connect_lazy("postgres://localhost/oms_test_never_connects")
            .expect("connect_lazy never actually connects");
        let symbology = Identifier::new(OpenFigiClient::new(None), InMemoryCache::new());
        let mut registry = BrokerRegistry::new();
        registry.register_alpaca("PAPER", sample_alpaca_adapter());
        // Nothing in these tests drains either channel — they only exercise the
        // registry swap and the stream/doorbell registries, never a real fill or
        // a real quote.
        let (quote_tx, _quote_rx) = mpsc::channel(1);
        AppState::new(
            pool,
            "test-admin-token".to_string(),
            false,
            registry,
            None,
            symbology,
            StreamHealthRegistry::new(),
            None,
            quote_tx,
        )
    }

    /// `AlpacaAdapter::new` just builds a `reqwest::Client` — no network I/O.
    fn sample_alpaca_adapter() -> Arc<AlpacaAdapter> {
        Arc::new(AlpacaAdapter::new(
            "key".to_string(),
            "secret".to_string(),
            "PAPER",
        ))
    }

    /// A reader that already resolved an adapter must keep working across a swap.
    /// This is the property that makes reload safe on the order path: an order in
    /// flight holds its own `Arc` and finishes against the adapter it started
    /// with, rather than having it replaced underneath.
    ///
    /// `#[tokio::test]`, not `#[test]`: `PgPool::connect_lazy` spawns a background
    /// maintenance task at construction even though it never actually connects, so
    /// it needs a Tokio context to exist in.
    #[tokio::test]
    async fn a_held_adapter_survives_a_swap() {
        let state = sample_state();

        let before = state.registry();
        let had_alpaca = before.get_alpaca("PAPER").is_some();

        state.swap_registry(BrokerRegistry::new()); // empty: everything removed

        // The guard taken before the swap still sees the old registry.
        assert_eq!(before.get_alpaca("PAPER").is_some(), had_alpaca);
        // A guard taken after sees the new one.
        assert!(state.registry().get_alpaca("PAPER").is_none());
    }

    /// Two swaps in a row must both land — a writer must not be able to publish a
    /// stale registry over a newer one.
    #[tokio::test]
    async fn the_last_swap_wins() {
        let state = sample_state();
        state.swap_registry(BrokerRegistry::new());
        let mut second = BrokerRegistry::new();
        second.register_alpaca("LIVE", sample_alpaca_adapter());
        state.swap_registry(second);
        assert!(state.registry().get_alpaca("LIVE").is_some());
    }

    /// Aborting a feed must actually stop it, and re-inserting under the same
    /// code must not leave the old task running — two Databento sessions would
    /// double-subscribe and double-count marks.
    #[tokio::test]
    async fn replacing_a_stream_aborts_the_previous_one() {
        let reg = StreamRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

        let first = tokio::spawn(async move {
            // Hold the sender until aborted.
            let _tx = tx;
            std::future::pending::<()>().await;
        });
        reg.insert("databento-opra", first);

        assert!(reg.abort_and_remove("databento-opra").await);

        // The sender was dropped with the task, so the channel closes — this is
        // the actual proof the task was aborted, not just unregistered.
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected),
            "stop must finish cancellation before returning, not at a later await");
        // And the map entry itself is gone, not just the task inside it left
        // to dangle — reached via the private field directly (this test module
        // is a descendant of `app_state`, so it sees what `codes()` used to
        // expose) now that the diagnostics-only accessor is gone.
        assert!(reg.handles.lock().unwrap().is_empty(), "abort_and_remove must also remove the map entry");
        assert!(!reg.abort_and_remove("databento-opra").await);
    }

    /// Replacing a feed's doorbell must overwrite its entry, not add a second
    /// one — otherwise a restarted feed's old, now-dead sender keeps
    /// accumulating on every reload, and `ring_all` keeps trying (harmlessly,
    /// but pointlessly) to wake a receiver nothing is listening on anymore.
    #[tokio::test]
    async fn replacing_a_doorbell_reaches_only_the_new_channel() {
        let reg = DoorbellRegistry::new();
        let (tx1, mut rx1) = tokio::sync::mpsc::channel::<()>(1);
        let (tx2, mut rx2) = tokio::sync::mpsc::channel::<()>(1);

        reg.set("databento-opra", tx1);
        reg.set("databento-opra", tx2);

        reg.ring_all();

        assert!(rx2.try_recv().is_ok(), "the new channel should have been rung");
        assert!(rx1.try_recv().is_err(), "the old channel should not have been rung");
    }
}
