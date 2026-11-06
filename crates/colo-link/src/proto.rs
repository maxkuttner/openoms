//! The control link's messages: JSON, tagged by `type`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use colo_risk::Limits;

/// Both sides heartbeat this often.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Position {
    pub symbol: String,
    pub qty: f64,
}

/// OMS → strategy. `State` is always the first message on a connection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToStrategy {
    State { active: bool, limits: Limits, limits_version: u64, positions: Vec<Position>, dead_man_secs: u64 },
    Limits { limits: Limits, limits_version: u64 },
    Positions { positions: Vec<Position> },
    Kill,
    Resume,
    Heartbeat,
}

/// Strategy → OMS.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromStrategy {
    AckLimits { limits_version: u64 },
    AckKill,
    Heartbeat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_tagged_json() {
        let ack: FromStrategy = serde_json::from_str(r#"{"type":"ack_limits","limits_version":7}"#).unwrap();
        assert_eq!(ack, FromStrategy::AckLimits { limits_version: 7 });
        assert_eq!(serde_json::to_string(&FromStrategy::AckKill).unwrap(), r#"{"type":"ack_kill"}"#);
        let state = ToStrategy::State { active: true, limits: Default::default(), limits_version: 3, positions: vec![Position { symbol: "BTCUSDT".into(), qty: 0.5 }], dead_man_secs: 5 };
        let back: ToStrategy = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(back, state);
    }
}
