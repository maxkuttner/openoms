//! Previous-close sources, one per connection that has a snapshot endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::BrokerError;

/// A connection's previous-close source: the price side of a connector that
/// shares the order side's credential.
#[async_trait::async_trait]
pub trait DailyStats: Send + Sync {
    /// (symbol, previous close) for the symbols it knows; others are left out.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError>;
}

/// Previous-close sources keyed by (broker_code, environment), like
/// [`crate::adapter::BrokerRegistry`] and published beside it.
#[derive(Default, Clone)]
pub struct DailyStatsRegistry {
    sources: HashMap<(String, String), Arc<dyn DailyStats>>,
}

impl DailyStatsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, broker_code: &str, environment: &str, source: Arc<dyn DailyStats>) {
        self.sources.insert((broker_code.to_string(), environment.to_string()), source);
    }

    pub fn get(&self, broker_code: &str, environment: &str) -> Option<Arc<dyn DailyStats>> {
        self.sources.get(&(broker_code.to_string(), environment.to_string())).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &Arc<dyn DailyStats>)> {
        self.sources.iter()
    }
}
