//! The pure, deterministic core of a co-located runtime: admission,
//! reservations and order/position reduction. No clock, venue or database
//! calls live here; the runner supplies monotonic time and performs I/O.

mod assets;
mod book;
pub mod inventory;
pub mod control;
pub mod journal;
pub mod recovery;
pub mod reporter;
pub mod runner;

pub use journal::{Journal, JournalRecord, SequencedRecord};

pub use assets::{AssetLedger, Reservation};
pub use control::{ControlAction, ControlState, RuntimeState};
pub use recovery::{recover, RecoveredBook};
pub use reporter::Reporter;
pub use runner::{Runtime, RuntimeConfig};
pub use book::{
    Book, BookCheckpoint, CancelRequest, InstrumentPrices, Intent, PreparedOrder, Prices, Submission,
};
pub use gateway::runtime::{
    AccountIdentity, AssetBalance, InstrumentRules, Operation, OrderType, RuntimeEvent,
    RuntimeOrder, RuntimeOrderState, Side, StreamState, TimeInForce, Trade, TradeKey,
    TradeProvenance, VenueCapabilities, VenueEvent,
};

/// Errors with distinct operator meanings: a risk rejection is not an
/// unavailable authority, and incomplete recovery is not corrupt state.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeError {
    Rejected(risk::Reject),
    InvalidIntent(String),
    InsufficientAssets { asset: String, required: f64, available: f64 },
    RateLimited { limit: u32 },
    UnavailableAuthority,
    IncompleteRecovery(String),
    CorruptState(String),
    UnknownInstrument(String),
    UnknownOrder(String),
    OrderNotWorking(String),
    InvalidConfig(String),
    DuplicateOrder(String),
    JournalIo(String),
    NotReady(String),
    KillLatched,
    StaleAuthority,
    InvalidControl(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(r) => write!(f, "risk rejected: {r}"),
            Self::InvalidIntent(msg) => write!(f, "invalid intent: {msg}"),
            Self::InsufficientAssets { asset, required, available } => {
                write!(f, "insufficient {asset}: need {required}, have {available}")
            }
            Self::RateLimited { limit } => write!(f, "rate limited at {limit}/s"),
            Self::UnavailableAuthority => write!(f, "execution authority unavailable"),
            Self::IncompleteRecovery(msg) => write!(f, "incomplete recovery: {msg}"),
            Self::CorruptState(msg) => write!(f, "corrupt state: {msg}"),
            Self::UnknownInstrument(symbol) => write!(f, "unknown instrument {symbol}"),
            Self::UnknownOrder(id) => write!(f, "unknown order {id}"),
            Self::OrderNotWorking(id) => write!(f, "order {id} is not working"),
            Self::InvalidConfig(msg) => write!(f, "invalid configuration: {msg}"),
            Self::DuplicateOrder(id) => write!(f, "duplicate order id {id}"),
            Self::JournalIo(msg) => write!(f, "journal I/O failed: {msg}"),
            Self::NotReady(msg) => write!(f, "runtime is not ready: {msg}"),
            Self::KillLatched => write!(f, "persistent kill latch is set"),
            Self::StaleAuthority => write!(f, "authority is stale or expired"),
            Self::InvalidControl(msg) => write!(f, "invalid control message: {msg}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<risk::Reject> for RuntimeError {
    fn from(value: risk::Reject) -> Self {
        Self::Rejected(value)
    }
}
