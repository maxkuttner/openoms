use std::sync::Arc;
use arc_swap::ArcSwap;
use sqlx::PgPool;
use symbology::{Identifier, InMemoryCache, OpenFigiClient};

use crate::adapters::BrokerRegistry;
use crate::kafka::KafkaClient;
use crate::marks::MarkStore;
use crate::stream_health::StreamHealthRegistry;

/// The OpenFIGI-backed instrument identification engine, with an in-memory cache.
pub type SymbologyEngine = Identifier<OpenFigiClient, InMemoryCache>;

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
    kafka: Option<KafkaClient>,
    symbology: Arc<SymbologyEngine>,
    stream_health: StreamHealthRegistry,
    marks: MarkStore,
}

impl AppState {
    pub fn new(
        pool: PgPool,
        admin_token: String,
        admin_auth_enabled: bool,
        registry: BrokerRegistry,
        kafka: Option<KafkaClient>,
        symbology: SymbologyEngine,
        stream_health: StreamHealthRegistry,
    ) -> Self {
        Self {
            pool,
            admin_token,
            admin_auth_enabled,
            registry: Arc::new(ArcSwap::from_pointee(registry)),
            kafka,
            symbology: Arc::new(symbology),
            stream_health,
            marks: MarkStore::new(),
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
    /// anything already holding an adapter is unaffected.
    ///
    /// Not yet called from production code — the reload endpoint that calls this
    /// lands in a later task. Exercised directly by the tests below.
    #[allow(dead_code)]
    pub fn swap_registry(&self, next: BrokerRegistry) {
        self.registry.store(Arc::new(next));
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
        AppState::new(
            pool,
            "test-admin-token".to_string(),
            false,
            registry,
            None,
            symbology,
            StreamHealthRegistry::new(),
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
}
