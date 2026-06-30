use std::collections::HashMap;
use std::sync::Mutex;

use crate::identity::Resolution;
use crate::query::InstrumentQuery;

/// Cache of prior identifications, keyed by the query, to avoid re-hitting OpenFIGI.
pub trait IdentityCache: Send + Sync {
    fn get(&self, q: &InstrumentQuery) -> Option<Resolution>;
    fn put(&self, q: &InstrumentQuery, res: &Resolution);
}

/// A no-op cache (always misses).
pub struct NoCache;

impl IdentityCache for NoCache {
    fn get(&self, _: &InstrumentQuery) -> Option<Resolution> {
        None
    }
    fn put(&self, _: &InstrumentQuery, _: &Resolution) {}
}

/// Simple process-local in-memory cache.
#[derive(Default)]
pub struct InMemoryCache {
    map: Mutex<HashMap<InstrumentQuery, Resolution>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdentityCache for InMemoryCache {
    fn get(&self, q: &InstrumentQuery) -> Option<Resolution> {
        self.map.lock().unwrap().get(q).cloned()
    }
    fn put(&self, q: &InstrumentQuery, res: &Resolution) {
        self.map.lock().unwrap().insert(q.clone(), res.clone());
    }
}
