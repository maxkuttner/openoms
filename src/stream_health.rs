//! Live connection health for the broker/exchange WebSocket streams.
//!
//! Each stream task (Alpaca, Binance, …) reports its state into a shared,
//! in-memory registry so the admin API can answer "which sockets are live?".
//! State is process-local and ephemeral — it reflects the currently running
//! streams, not anything persisted.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Connection state of a single broker stream.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamState {
    /// Dialing / authenticating — not yet receiving user-data.
    Connecting,
    /// Authenticated and subscribed; the socket is delivering events.
    Live,
    /// Disconnected or errored; a reconnect is pending.
    Down,
}

/// A point-in-time snapshot of one stream's health, returned by the API.
#[derive(Clone, Serialize)]
pub struct StreamHealth {
    pub broker_code: String,
    pub environment: String,
    pub state: StreamState,
    /// When the current live session was established (cleared on disconnect).
    pub connected_since: Option<DateTime<Utc>>,
    /// Last time any frame (incl. keepalive) arrived — freshness of "live".
    pub last_event_at: Option<DateTime<Utc>>,
    /// Last disconnect/error reason, kept for display after a drop.
    pub last_error: Option<String>,
}

type Key = (String, String); // (broker_code, environment)

/// Shared registry of stream health, cheap to clone (Arc inside).
#[derive(Clone, Default)]
pub struct StreamHealthRegistry {
    inner: Arc<RwLock<HashMap<Key, StreamHealth>>>,
}

impl StreamHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand a stream task its own updater. Seeds a `Connecting` entry so the
    /// broker shows up in the API from the moment the task starts.
    pub fn handle(&self, broker_code: &str, environment: &str) -> StreamHandle {
        let key = (broker_code.to_string(), environment.to_string());
        if let Ok(mut map) = self.inner.write() {
            map.entry(key.clone()).or_insert_with(|| StreamHealth {
                broker_code: broker_code.to_string(),
                environment: environment.to_string(),
                state: StreamState::Connecting,
                connected_since: None,
                last_event_at: None,
                last_error: None,
            });
        }
        StreamHandle { inner: self.inner.clone(), key }
    }

    /// All known streams, sorted for stable rendering.
    pub fn snapshot(&self) -> Vec<StreamHealth> {
        let mut out: Vec<StreamHealth> = self
            .inner
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        out.sort_by(|a, b| {
            (a.broker_code.as_str(), a.environment.as_str())
                .cmp(&(b.broker_code.as_str(), b.environment.as_str()))
        });
        out
    }
}

/// Per-stream updater handed to each `run()` task.
#[derive(Clone)]
pub struct StreamHandle {
    inner: Arc<RwLock<HashMap<Key, StreamHealth>>>,
    key: Key,
}

impl StreamHandle {
    fn mutate(&self, f: impl FnOnce(&mut StreamHealth)) {
        if let Ok(mut map) = self.inner.write() {
            if let Some(h) = map.get_mut(&self.key) {
                f(h);
            }
        }
    }

    /// Dialing / (re)authenticating.
    pub fn set_connecting(&self) {
        self.mutate(|h| {
            h.state = StreamState::Connecting;
            h.connected_since = None;
        });
    }

    /// Authenticated + subscribed; the socket is live.
    pub fn set_live(&self) {
        let now = Utc::now();
        self.mutate(|h| {
            h.state = StreamState::Live;
            h.connected_since = Some(now);
            h.last_event_at = Some(now);
            h.last_error = None;
        });
    }

    /// Disconnected or errored; reconnect pending.
    pub fn set_down(&self, error: impl Into<String>) {
        self.mutate(|h| {
            h.state = StreamState::Down;
            h.connected_since = None;
            h.last_error = Some(error.into());
        });
    }

    /// A frame arrived — refresh liveness (any message, including keepalives).
    pub fn record_event(&self) {
        let now = Utc::now();
        self.mutate(|h| h.last_event_at = Some(now));
    }
}

/// Lets a `dataprovider` feed report liveness without that crate knowing about
/// this registry.
impl dataprovider::FeedHealth for StreamHandle {
    fn on_connected(&self) {
        self.set_live();
    }

    fn on_event(&self) {
        self.record_event();
    }
}
