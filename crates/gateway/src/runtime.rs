//! The richer venue contract a co-located runtime needs: instrument rules,
//! balances, complete order/execution recovery and a push stream.
//!
//! The existing [`BrokerAdapter`] stays the path-① order transport. This trait
//! adds the recovery and reporting surface without depending on the OMS or the
//! runtime crate, so both the local reducer and the link layer can share it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::adapter::{BrokerAdapter, BrokerError, BrokerOrderResponse};

/// The account a runtime is authoritative for. Operator verification supplies
/// this before bootstrap; a venue response alone cannot invent it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountIdentity {
    pub account_id: Uuid,
    pub venue: String,
    pub environment: String,
    pub external_account_ref: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
}

/// An operation the venue can perform. Anything absent must not be simulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Place,
    Cancel,
    Amend,
}

/// Explicit capability declarations: an unsupported feature is named, never
/// inferred from a successful order call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueCapabilities {
    pub cancel_on_disconnect: bool,
    pub atomic_cancel_replace: bool,
    pub open_orders_snapshot: bool,
    pub order_lookup: bool,
    pub trade_history: bool,
    pub balance_snapshot: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentRules {
    /// Canonical identity (`Symbol@Venue` symbol).
    pub symbol: String,
    pub venue: String,
    /// The venue's own routing symbol.
    pub venue_symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub tick_size: f64,
    pub step_size: f64,
    pub multiplier: f64,
    pub min_quantity: Option<f64>,
    pub max_quantity: Option<f64>,
    pub min_notional: Option<f64>,
    pub max_notional: Option<f64>,
    pub operations: Vec<Operation>,
    pub capabilities: VenueCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetBalance {
    pub asset: String,
    pub free: f64,
    pub locked: f64,
}

impl AssetBalance {
    pub fn total(&self) -> f64 {
        self.free + self.locked
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOrderState {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOrder {
    pub client_order_id: String,
    pub venue_order_id: String,
    pub symbol: String,
    pub side: Side,
    pub original_quantity: f64,
    pub executed_quantity: f64,
    pub leaves_quantity: f64,
    pub limit_price: Option<f64>,
    pub state: RuntimeOrderState,
    pub original_time: DateTime<Utc>,
}

/// Identity that makes one fill unique across venues and accounts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TradeKey {
    pub venue: String,
    pub account_id: Uuid,
    pub environment: String,
    pub symbol: String,
    pub trade_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fee {
    pub amount: f64,
    pub currency: String,
}

/// Where a trade observation came from; replay and stream copies still share a
/// [`TradeKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeProvenance {
    Stream,
    History,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub key: TradeKey,
    pub venue_order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub quantity: f64,
    pub price: f64,
    pub executed_at: DateTime<Utc>,
    /// `None` means the venue did not report the fee; it is not a zero fee.
    pub fee: Option<Fee>,
    pub provenance: TradeProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Live,
    Stale,
    Closed,
}

/// One venue observation. No wall-clock ordering is implied across variants;
/// the runtime assigns transport order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VenueEvent {
    Order(RuntimeOrder),
    Trade(Trade),
    Balances { balances: Vec<AssetBalance> },
    Health { state: StreamState, observed_at: DateTime<Utc> },
}

/// Per-symbol position in the venue's execution history. A missing both
/// fields means no overlap-safe recovery boundary has been supplied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCursor {
    /// Last applied trade id, inclusive.
    pub last_trade_id: Option<i64>,
    /// History start when no trade id is known yet.
    pub start_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCursor {
    pub positions: BTreeMap<String, SymbolCursor>,
}

impl ExecutionCursor {
    pub fn symbol(&self, symbol: &str) -> Option<&SymbolCursor> {
        self.positions.get(symbol)
    }

    pub fn at_start(symbol: &str, at: DateTime<Utc>) -> Self {
        let mut positions = BTreeMap::new();
        positions.insert(symbol.to_string(), SymbolCursor {
            last_trade_id: None,
            start_time: Some(at),
        });
        Self { positions }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPage {
    pub trades: Vec<Trade>,
    /// Cursor to resume after this page.
    pub next: ExecutionCursor,
    /// False when the venue may have more history or no safe boundary existed.
    pub complete: bool,
    /// Symbols whose history could not be established.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderIntent {
    pub client_order_id: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub order_type: OrderType,
    pub limit_price: Option<f64>,
    pub time_in_force: TimeInForce,
}

/// Local acceptance is not venue acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubmitOutcome {
    Accepted { venue_order_id: String },
    /// The venue may have taken the order. Never retried under a new identity.
    Unknown { reason: String },
    Rejected { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommand {
    Policy,
    Authority,
    Kill,
    Enable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryCheckpoint {
    pub sequence: u64,
    pub complete: bool,
    pub missing: Vec<String>,
}

/// One event the runtime reports to the OMS or journals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Admitted {
        account: AccountIdentity,
        intent: OrderIntent,
    },
    SubmitOutcome {
        account: AccountIdentity,
        client_order_id: String,
        outcome: SubmitOutcome,
    },
    Venue {
        account: AccountIdentity,
        event: VenueEvent,
    },
    Health {
        account: AccountIdentity,
        state: StreamState,
        observed_at: DateTime<Utc>,
    },
    Control {
        account: AccountIdentity,
        command: ControlCommand,
        epoch: u64,
        applied: bool,
    },
    Recovery {
        account: AccountIdentity,
        checkpoint: RecoveryCheckpoint,
    },
}

/// Map a submit result without turning an ambiguous failure into a rejection.
pub fn submit_outcome(result: Result<BrokerOrderResponse, BrokerError>) -> SubmitOutcome {
    match result {
        Ok(r) => SubmitOutcome::Accepted { venue_order_id: r.external_order_id },
        Err(BrokerError::BrokerRejected(reason)) => SubmitOutcome::Rejected { reason },
        Err(e) => SubmitOutcome::Unknown { reason: e.to_string() },
    }
}

/// The recovery/reporting contract. Implementors also satisfy the path-①
/// [`BrokerAdapter`] so the same credential can send orders.
#[async_trait::async_trait]
pub trait RuntimeVenue: BrokerAdapter {
    async fn metadata(&self, symbols: &[String]) -> Result<Vec<InstrumentRules>, BrokerError>;
    async fn balances(&self) -> Result<Vec<AssetBalance>, BrokerError>;
    async fn snapshot_orders(&self) -> Result<Vec<RuntimeOrder>, BrokerError>;
    async fn executions(&self, cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError>;
    async fn lookup(&self, client_id: &str, symbol: &str) -> Result<RuntimeOrder, BrokerError>;
    /// Establish the push stream. Events, including health after the initial
    /// connection, arrive on `tx`; a closed receiver ends the stream.
    async fn subscribe(&self, tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_5xx_submit_is_unknown_not_rejected() {
        assert_eq!(
            submit_outcome(Err(BrokerError::Network("timeout".into()))),
            SubmitOutcome::Unknown { reason: "network error: timeout".into() }
        );
        assert_eq!(
            submit_outcome(Err(BrokerError::BrokerRejected("bad qty".into()))),
            SubmitOutcome::Rejected { reason: "bad qty".into() }
        );
    }

    #[test]
    fn a_trade_key_is_scoped_by_account_and_symbol() {
        let account = Uuid::new_v4();
        let key = |symbol: &str, account_id| TradeKey {
            venue: "BINANCE".into(),
            account_id,
            environment: "LIVE".into(),
            symbol: symbol.into(),
            trade_id: "7".into(),
        };
        assert_ne!(key("BTCUSDT", account), key("ETHUSDT", account));
        assert_ne!(key("BTCUSDT", account), key("BTCUSDT", Uuid::new_v4()));
    }

    #[test]
    fn unknown_fee_is_not_zero() {
        let no_fee: Option<Fee> = None;
        assert_ne!(no_fee, Some(Fee { amount: 0.0, currency: "BNB".into() }));
    }
}
