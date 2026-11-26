//! Live connection health for every broker/exchange stream: the WebSocket
//! order and quote streams, the FIX sessions and the IBKR TWS session.
//!
//! Each stream task reports its state into a shared, in-memory registry so the
//! admin API can answer "which connections are live?". State is process-local
//! and ephemeral — it reflects the currently running streams, not anything
//! persisted.
//!
//! Every stream type uses the same four states (see [`StreamState`]) and the
//! same liveness clock: `last_checked_at` moves whenever the stream proves it
//! is alive (a frame, a keepalive reply, a ping answer) or makes an attempt.
//! A `live` stream whose clock is older than two `heartbeat_secs` reads `down`
//! in [`StreamHealthRegistry::snapshot`] — a stalled socket cannot stay green.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Expected liveness interval for a stream that does not say otherwise.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 60;

/// Connection state of a single stream. The same set for every stream type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamState {
    /// An attempt is in progress right now (dialing, authenticating,
    /// subscribing, or ibapi reconnecting a dropped IB Gateway socket).
    Connecting,
    /// Connected and the heartbeat is fresh — or idle (a quote feed with
    /// nothing to price), which `last_error` then says.
    Live,
    /// The last attempt or heartbeat failed; a retry is scheduled
    /// (`next_retry_at` when the stream knows when).
    Down,
    /// Will not retry until the user changes credentials or configuration
    /// (no gateway address, the address on the other environment's port,
    /// a gateway logged into the other kind of account). A disabled or
    /// removed connection has no entry at all.
    Stopped,
}

/// What a stream is for — the axis the cockpit splits on. A data `Feed` delivers
/// market data (quotes); an `Execution` stream carries a broker's order/fill
/// updates. The same vendor can run both (Binance feed + Binance execution).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Feed,
    Execution,
}

/// The wire protocol a stream runs over. Lets the cockpit distinguish a FIX
/// order-entry session from a WebSocket one at a glance.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamTransport {
    WebSocket,
    Fix,
}

fn default_heartbeat_secs() -> u64 {
    DEFAULT_HEARTBEAT_SECS
}

/// A point-in-time snapshot of one stream's health, returned by the API.
#[derive(Clone, Serialize, Deserialize)]
pub struct StreamHealth {
    pub broker_code: String,
    pub environment: String,
    pub kind: StreamKind,
    pub transport: StreamTransport,
    pub state: StreamState,
    /// When the current live session was established (cleared on disconnect).
    pub connected_since: Option<DateTime<Utc>>,
    /// Last order/fill event (or quote, for a feed). Not the freshness signal:
    /// a quiet but healthy stream has none for hours.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Why the stream is connecting, down or stopped; cleared when it goes live.
    pub last_error: Option<String>,
    /// Last time the stream verified it is alive or made an attempt, success or failure.
    #[serde(default)]
    pub last_checked_at: Option<DateTime<Utc>>,
    /// When the next attempt runs; set only while `down`.
    #[serde(default)]
    pub next_retry_at: Option<DateTime<Utc>>,
    /// How often a live stream is expected to check; `live` without a check
    /// for twice this reads `down`.
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

impl StreamHealth {
    /// This health as of `now`: a live stream whose last check is older than
    /// two heartbeats is reported down.
    fn as_of(mut self, now: DateTime<Utc>) -> Self {
        if self.state == StreamState::Live {
            let since = self.last_checked_at.or(self.connected_since);
            if let Some(since) = since {
                let silent = (now - since).num_seconds();
                if silent > 2 * self.heartbeat_secs as i64 {
                    self.state = StreamState::Down;
                    self.last_error = Some(format!("no heartbeat for {silent}s"));
                }
            }
        }
        self
    }
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

    /// Hand a WebSocket stream task its own updater. Seeds a `Connecting` entry so
    /// the stream shows up in the API from the moment the task starts.
    pub fn handle(&self, broker_code: &str, environment: &str, kind: StreamKind) -> StreamHandle {
        self.handle_with_transport(broker_code, environment, kind, StreamTransport::WebSocket)
    }

    /// Hand a FIX order-entry session its updater (always an `Execution` stream).
    /// Call this only once the session is actually starting, so an unconfigured
    /// broker doesn't leave a phantom `Connecting` entry.
    pub fn fix_handle(&self, broker_code: &str, environment: &str) -> StreamHandle {
        self.handle_with_transport(broker_code, environment, StreamKind::Execution, StreamTransport::Fix)
    }

    fn handle_with_transport(
        &self,
        broker_code: &str,
        environment: &str,
        kind: StreamKind,
        transport: StreamTransport,
    ) -> StreamHandle {
        let key = (broker_code.to_string(), environment.to_string());
        if let Ok(mut map) = self.inner.write() {
            map.entry(key.clone()).or_insert_with(|| StreamHealth {
                broker_code: broker_code.to_string(),
                environment: environment.to_string(),
                kind,
                transport,
                state: StreamState::Connecting,
                connected_since: None,
                last_event_at: None,
                last_error: None,
                last_checked_at: None,
                next_retry_at: None,
                heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            });
        }
        StreamHandle { inner: self.inner.clone(), key }
    }

    /// Forgets a stream: its connection was disabled, unconfigured or removed,
    /// so it has no health to show (not "stopped", which asks for a fix).
    pub fn remove(&self, broker_code: &str, environment: &str, kind: StreamKind) {
        if let Ok(mut map) = self.inner.write() {
            let key = (broker_code.to_string(), environment.to_string());
            if map.get(&key).is_some_and(|h| h.kind == kind) {
                map.remove(&key);
            }
        }
    }

    /// All known streams as of now, sorted for stable rendering. A live stream
    /// that has not checked for two heartbeats reads `down`.
    pub fn snapshot(&self) -> Vec<StreamHealth> {
        self.snapshot_at(Utc::now())
    }

    /// [`Self::snapshot`] as of `now`.
    pub fn snapshot_at(&self, now: DateTime<Utc>) -> Vec<StreamHealth> {
        let mut out: Vec<StreamHealth> = self
            .inner
            .read()
            .map(|m| m.values().cloned().map(|h| h.as_of(now)).collect())
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

    /// Whether a FIX session owns this entry (it marks itself live on logon).
    pub fn is_fix_session(&self) -> bool {
        self.inner
            .read()
            .map(|m| m.get(&self.key).is_some_and(|h| matches!(h.transport, StreamTransport::Fix)))
            .unwrap_or(false)
    }

    /// The stream's expected liveness interval (its keepalive or heartbeat cadence).
    pub fn set_heartbeat_secs(&self, secs: u64) {
        self.mutate(|h| h.heartbeat_secs = secs.max(1));
    }

    fn enter(&self, state: StreamState, reason: Option<String>, retry_at: Option<DateTime<Utc>>) {
        let now = Utc::now();
        self.mutate(|h| {
            h.state = state;
            h.connected_since = None;
            h.last_error = reason;
            h.last_checked_at = Some(now);
            h.next_retry_at = retry_at;
        });
    }

    /// An attempt starts now (dialing / authenticating).
    pub fn set_connecting(&self) {
        self.enter(StreamState::Connecting, None, None);
    }

    /// An attempt is in progress and this is where it is (connecting to the
    /// FIX host, IB Gateway's socket reconnecting…); shown until it goes live.
    pub fn set_connecting_because(&self, message: impl Into<String>) {
        self.enter(StreamState::Connecting, Some(message.into()), None);
    }

    /// Connected; the stream is live.
    pub fn set_live(&self) {
        let now = Utc::now();
        self.mutate(|h| {
            h.state = StreamState::Live;
            h.connected_since = Some(now);
            h.last_checked_at = Some(now);
            h.last_error = None;
            h.next_retry_at = None;
        });
    }

    /// Nothing to connect for (a quote feed with nothing held): live, with
    /// `reason` shown. The idle loop keeps checking in so it does not go stale.
    pub fn set_idle(&self, reason: impl Into<String>) {
        let now = Utc::now();
        let reason = reason.into();
        self.mutate(|h| {
            h.state = StreamState::Live;
            h.connected_since = None;
            h.last_checked_at = Some(now);
            h.last_error = Some(reason);
            h.next_retry_at = None;
        });
    }

    /// Disconnected or failed; something else brings it back (the client's own
    /// reconnect, the gateway's) at a time this stream does not know.
    pub fn set_down(&self, error: impl Into<String>) {
        self.enter(StreamState::Down, Some(error.into()), None);
    }

    /// The attempt failed; the next one runs in `retry_in`.
    pub fn set_down_retrying(&self, error: impl Into<String>, retry_in: Duration) {
        let at = Utc::now() + chrono::Duration::from_std(retry_in).unwrap_or(chrono::Duration::zero());
        self.enter(StreamState::Down, Some(error.into()), Some(at));
    }

    /// Will not retry until the user changes credentials or configuration.
    pub fn set_stopped(&self, reason: impl Into<String>) {
        self.enter(StreamState::Stopped, Some(reason.into()), None);
    }

    /// The stream verified it is alive (a keepalive reply, a ping answer)
    /// without an order/fill event.
    pub fn record_check(&self) {
        let now = Utc::now();
        self.mutate(|h| h.last_checked_at = Some(now));
    }

    /// An order/fill event (a quote, for a feed) arrived — also proof of life.
    pub fn record_event(&self) {
        let now = Utc::now();
        self.mutate(|h| {
            h.last_event_at = Some(now);
            h.last_checked_at = Some(now);
        });
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

    fn on_check(&self) {
        self.record_check();
    }

    fn on_heartbeat_every(&self, secs: u64) {
        self.set_heartbeat_secs(secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(reg: &StreamHealthRegistry) -> StreamHealth {
        reg.snapshot().remove(0)
    }

    #[test]
    fn connecting_can_say_why_and_going_live_clears_it() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        h.set_connecting_because("IB Gateway is starting");
        let s = one(&reg);
        assert!(s.state == StreamState::Connecting);
        assert_eq!(s.last_error.as_deref(), Some("IB Gateway is starting"));
        h.set_live();
        assert_eq!(one(&reg).last_error, None);
    }

    #[test]
    fn a_new_stream_is_connecting_with_the_default_heartbeat() {
        let reg = StreamHealthRegistry::new();
        reg.handle("ALPACA", "PAPER", StreamKind::Execution);
        let s = one(&reg);
        assert!(s.state == StreamState::Connecting);
        assert_eq!(s.heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
        assert_eq!((s.last_checked_at, s.next_retry_at), (None, None));
    }

    #[test]
    fn an_attempt_is_a_check_and_drops_a_stale_reason() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("ALPACA", "PAPER", StreamKind::Execution);
        h.set_down_retrying("refused", Duration::from_secs(4));
        h.set_connecting();
        let s = one(&reg);
        assert!(s.state == StreamState::Connecting);
        assert_eq!((s.last_error, s.next_retry_at), (None, None), "connecting is about this attempt only");
        assert!(s.last_checked_at.is_some());
    }

    #[test]
    fn a_failed_attempt_is_down_with_when_it_retries() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        h.set_live();
        let before = Utc::now();
        h.set_down_retrying("IB Gateway at gw:4004 not reachable", Duration::from_secs(30));
        let s = one(&reg);
        assert!(s.state == StreamState::Down);
        assert_eq!(s.last_error.as_deref(), Some("IB Gateway at gw:4004 not reachable"));
        assert_eq!(s.connected_since, None);
        let retry = s.next_retry_at.expect("a retry is scheduled");
        assert!(retry >= before + chrono::Duration::seconds(30) && retry <= Utc::now() + chrono::Duration::seconds(30));
        assert!(s.last_checked_at.unwrap() >= before);
        // Plain down (no retry time known) carries none.
        h.set_down("lost");
        assert_eq!(one(&reg).next_retry_at, None);
    }

    #[test]
    fn stopped_will_not_retry() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        h.set_down_retrying("x", Duration::from_secs(5));
        h.set_stopped("IB Gateway at gw:4004 is logged into a live account but this connection is PAPER");
        let s = one(&reg);
        assert!(s.state == StreamState::Stopped);
        assert_eq!(s.last_error.as_deref(), Some("IB Gateway at gw:4004 is logged into a live account but this connection is PAPER"));
        assert_eq!(s.next_retry_at, None);
        assert_eq!(serde_json::to_value(s.state).unwrap(), "stopped");
    }

    #[test]
    fn live_clears_the_retry_and_checks() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("OKX", "LIVE", StreamKind::Execution);
        h.set_down_retrying("x", Duration::from_secs(5));
        h.set_live();
        let s = one(&reg);
        assert!(s.state == StreamState::Live);
        assert_eq!((s.next_retry_at, s.last_error), (None, None));
        assert!(s.connected_since.is_some() && s.last_checked_at.is_some());
        assert_eq!(s.last_event_at, None, "going live is not an order/fill event");
    }

    #[test]
    fn a_check_moves_only_the_check_clock_an_event_moves_both() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        h.record_check();
        let s = one(&reg);
        assert!(s.last_checked_at.is_some() && s.last_event_at.is_none());
        h.record_event();
        let s = one(&reg);
        assert!(s.last_event_at.is_some() && s.last_checked_at >= s.last_event_at);
    }

    #[test]
    fn a_live_stream_without_a_check_for_two_heartbeats_reads_down() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        h.set_heartbeat_secs(30);
        h.set_live();
        let checked = one(&reg).last_checked_at.unwrap();
        // Exactly two heartbeats: still live.
        let s = reg.snapshot_at(checked + chrono::Duration::seconds(60)).remove(0);
        assert!(s.state == StreamState::Live);
        let s = reg.snapshot_at(checked + chrono::Duration::seconds(75)).remove(0);
        assert!(s.state == StreamState::Down);
        assert_eq!(s.last_error.as_deref(), Some("no heartbeat for 75s"));
        assert_eq!(s.heartbeat_secs, 30);
        // Only live streams go stale: down/stopped/connecting keep their own reason.
        h.set_stopped("set the Gateway address");
        let s = reg.snapshot_at(Utc::now() + chrono::Duration::seconds(600)).remove(0);
        assert!(s.state == StreamState::Stopped);
    }

    /// A disabled or removed connection has no stream: its entry goes, rather
    /// than lingering as a red "stopped" that asks for credentials.
    #[test]
    fn removing_a_stream_drops_its_entry_only() {
        let reg = StreamHealthRegistry::new();
        reg.handle("ALPACA", "PAPER", StreamKind::Execution).set_live();
        reg.handle("DATABENTO", "OPRA", StreamKind::Feed).set_live();
        reg.remove("ALPACA", "PAPER", StreamKind::Execution);
        reg.remove("DATABENTO", "OPRA", StreamKind::Execution); // other kind: kept
        let left: Vec<_> = reg.snapshot().into_iter().map(|h| h.broker_code).collect();
        assert_eq!(left, ["DATABENTO"]);
        reg.remove("DATABENTO", "OPRA", StreamKind::Feed);
        assert!(reg.snapshot().is_empty());
    }

    /// A feed with nothing to subscribe is idle, not mid-attempt: live with
    /// a reason, and it does not go stale while it keeps checking in.
    #[test]
    fn an_idle_stream_is_live_with_a_reason() {
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("BINANCE", "SPOT", StreamKind::Feed);
        h.set_idle("idle: nothing held to price");
        let s = one(&reg);
        assert!(s.state == StreamState::Live);
        assert_eq!(s.last_error.as_deref(), Some("idle: nothing held to price"));
        assert!(s.last_checked_at.is_some() && s.connected_since.is_none());
        h.set_live();
        assert_eq!(one(&reg).last_error, None);
    }

    #[test]
    fn a_feed_check_is_not_a_quote() {
        use dataprovider::FeedHealth;
        let reg = StreamHealthRegistry::new();
        let h = reg.handle("OKX", "SPOT", StreamKind::Feed);
        h.on_check();
        let s = one(&reg);
        assert!(s.last_checked_at.is_some() && s.last_event_at.is_none());
        h.on_event();
        assert!(one(&reg).last_event_at.is_some());
    }

    #[test]
    fn older_payloads_without_the_new_fields_still_parse() {
        let v = serde_json::json!({
            "broker_code": "ALPACA", "environment": "PAPER", "kind": "execution", "transport": "websocket",
            "state": "down", "connected_since": null, "last_event_at": null, "last_error": "x"
        });
        let s: StreamHealth = serde_json::from_value(v).unwrap();
        assert_eq!((s.heartbeat_secs, s.last_checked_at), (DEFAULT_HEARTBEAT_SECS, None));
    }
}
