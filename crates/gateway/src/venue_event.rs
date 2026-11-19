//! What a venue reports about orders and executions, in one shape for every
//! connector, and the pull side the engine uses to fill gaps.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::adapter::BrokerError;

/// One execution on a venue account, identified by `exec_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueFill {
    pub exec_id: String,
    pub venue_order_id: String,
    pub client_order_id: Option<String>,
    pub account_ref: String,
    pub symbol: String,
    pub buy: bool,
    pub qty: f64,
    pub price: f64,
    /// Amount and currency, when the venue reports the fee with the fill.
    pub fee: Option<(f64, String)>,
    pub executed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VenueOrderStatus {
    Working,
    Filled,
    Canceled,
    Rejected { reason: String },
    Expired,
}

/// The venue's current view of one order.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueOrderUpdate {
    pub venue_order_id: String,
    pub client_order_id: Option<String>,
    pub account_ref: String,
    pub symbol: String,
    pub buy: bool,
    pub order_type: String,
    pub qty: f64,
    pub limit_px: Option<f64>,
    pub status: VenueOrderStatus,
    pub cum_qty: f64,
    /// Venue update time.
    pub ts: Option<DateTime<Utc>>,
    /// When the venue created the order, where its API says.
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VenueEvent {
    Order(VenueOrderUpdate),
    Fill(VenueFill),
    Fee { exec_id: String, account_ref: String, amount: f64, currency: String },
    /// The stream is (re)connected: time to fill any gap.
    Connected,
    Disconnected,
}

/// Opaque position in a venue's execution history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct PullPage {
    pub fills: Vec<VenueFill>,
    pub next: Cursor,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PullHints {
    /// How often to poll for fills the stream may have missed.
    pub intraday_secs: u64,
    /// How far back the venue serves executions; `None` when unbounded.
    pub max_history: Option<chrono::Duration>,
    /// Executions can only be pulled one symbol (scope) at a time.
    pub per_symbol: bool,
}

#[async_trait]
pub trait ExecutionSource: Send + Sync {
    /// One page of fills after `from` (from the start of the scope's history
    /// when `None`). The source always drops fills executed before `since`,
    /// on every page. The caller picks the window: a scan started with no
    /// cursor (first pull, EOD sweep) passes the same anchored `since` on all
    /// its pages; a pull resuming from an established cursor passes
    /// `DateTime::<Utc>::MIN_UTC`, since everything after the cursor is new
    /// (including late-indexed trades stamped earlier).
    async fn fills_since(
        &self,
        account_ref: &str,
        scope: &str,
        from: Option<&Cursor>,
        since: DateTime<Utc>,
    ) -> Result<PullPage, BrokerError>;
    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError>;
    /// `[""]` unless `hints().per_symbol`.
    async fn scopes(&self, account_ref: &str) -> Result<Vec<String>, BrokerError>;
    /// Assets the account holds a non-zero balance of, so a per-symbol venue's
    /// caller can discover pairs it has no order on (a pair traded entirely
    /// while the stream was down). Costly on some venues: call it on reconnect
    /// and in the EOD sweep, not on every intraday pull. Empty by default.
    async fn held_assets(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(Vec::new())
    }
    fn hints(&self) -> PullHints;
}
