//! Previous-close cache, sibling to `MarkStore` (src/marks.rs) but a different
//! cadence and source: `MarkStore` is written continuously by the streaming
//! quote feeds, this is written every few minutes by `daily_stats_poller`
//! calling each broker adapter's own snapshot/24hr-stats endpoint. Kept
//! separate rather than folded into `Mark` because the two are populated
//! independently and one being absent must never block the other.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy)]
pub struct DailyStat {
    pub prev_close: f64,
    pub ts: DateTime<Utc>,
}

#[derive(Clone, Default)]
pub struct DailyStatsStore {
    inner: Arc<RwLock<HashMap<i64, DailyStat>>>,
}

impl DailyStatsStore {
    pub fn new() -> Self { Self::default() }

    pub fn set(&self, instrument_id: i64, prev_close: f64) {
        if let Ok(mut m) = self.inner.write() {
            m.insert(instrument_id, DailyStat { prev_close, ts: Utc::now() });
        }
    }

    pub fn get(&self, instrument_id: i64) -> Option<DailyStat> {
        self.inner.read().ok().and_then(|m| m.get(&instrument_id).copied())
    }

    pub fn get_all(&self) -> HashMap<i64, DailyStat> {
        self.inner.read().ok().map(|m| m.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get_round_trips() {
        let store = DailyStatsStore::new();
        store.set(42, 100.50);
        let stat = store.get(42).expect("just set");
        assert_eq!(stat.prev_close, 100.50);
    }

    #[test]
    fn missing_instrument_is_none_not_zero() {
        let store = DailyStatsStore::new();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn get_all_returns_every_entry() {
        let store = DailyStatsStore::new();
        store.set(1, 10.0);
        store.set(2, 20.0);
        let all = store.get_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[&1].prev_close, 10.0);
        assert_eq!(all[&2].prev_close, 20.0);
    }
}
