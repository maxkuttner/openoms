
use chrono::{DateTime, Utc};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub struct Mark {
    pub bid: f64,
    pub ask: f64,
    pub ts: DateTime<Utc>,
}

impl Mark {
    // mid-price of the mark
    pub fn mid(&self) -> f64 { (self.bid + self.ask) / 2.0 }
}

#[derive(Clone, Default)]
pub struct MarkStore {
    // create a atomic smart pointer to a multiple-readers single-writer hashmap
    // where the key is the instrument id and the value is the ``Mark`` struct
    inner: Arc<RwLock<HashMap<i64, Mark>>>,
}

impl MarkStore {
    pub fn new() -> Self { Self::default() }

    pub fn set(&self, instrument_id: i64, bid: f64, ask: f64) {
        // acquire a write lock on the inner hashmap and insert a mark
        if let Ok(mut m) = self.inner.write() {
            m.insert(instrument_id, Mark { bid, ask, ts: Utc::now() });
        }
    }

    pub fn get(&self, instrument_id: i64) -> Option<Mark> {
        self.inner.read().ok().and_then(|m| m.get(&instrument_id).copied())
    }
    
    pub fn get_all(&self) -> HashMap<i64, Mark> {
        // acquire a read lock on the inner hashmap and return a copy of it
        self.inner.read().ok().map(|m| m.clone()).unwrap_or_default()
    }
}




