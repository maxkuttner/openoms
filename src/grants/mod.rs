//! Trading grants: what a portfolio is allowed to trade.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One thing a portfolio is allowed to do. Stored as `{"kind", "params"}`.
///
/// Future shapes: `InstrumentClass { connection, class }` and
/// `Hours { tz, days, from, to }`.
///
/// A new kind touches four places: (1) a variant here, (2) its arm in
/// `admits`, (3) `KINDS` and `when_absent`, (4) a UI section in the cockpit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Grant {
    Venue { connection: String, account_ref: Option<String> },
}

/// What it means when a portfolio has no grant of a given kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    Deny,
    Allow,
}

/// Every grant kind the check enforces.
const KINDS: &[&str] = &["venue"];

/// The facts about an order that grants are checked against.
pub struct OrderFacts {
    pub connection: String,
    pub broker_code: String,
    // Not read by `Venue`; reserved for instrument-class and hours kinds.
    #[allow(dead_code)]
    pub instrument_class: Option<String>,
    #[allow(dead_code)]
    pub at: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Denied(pub String);

impl Grant {
    pub fn kind(&self) -> &'static str {
        match self {
            Grant::Venue { .. } => "venue",
        }
    }

    pub fn when_absent(kind: &str) -> Absent {
        match kind {
            "venue" => Absent::Deny,
            _ => Absent::Allow,
        }
    }

    fn admits(&self, f: &OrderFacts) -> bool {
        match self {
            Grant::Venue { connection, .. } => *connection == f.connection,
        }
    }
}

fn denial(kind: &str) -> Denied {
    Denied(
        match kind {
            "venue" => "venue not granted to this portfolio",
            _ => "not granted to this portfolio",
        }
        .to_string(),
    )
}

/// For each registered kind: if the portfolio holds grants of that kind, at
/// least one must admit the order; if it holds none, `when_absent` decides.
pub fn check(f: &OrderFacts, grants: &[Grant]) -> Result<(), Denied> {
    for kind in KINDS {
        let mut of_kind = grants.iter().filter(|g| g.kind() == *kind).peekable();
        let ok = if of_kind.peek().is_none() {
            Grant::when_absent(kind) == Absent::Allow
        } else {
            of_kind.any(|g| g.admits(f))
        };
        if !ok {
            return Err(denial(kind));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(conn: &str) -> OrderFacts {
        OrderFacts {
            connection: conn.into(),
            broker_code: "ALPACA".into(),
            instrument_class: None,
            at: Utc::now(),
        }
    }
    fn venue(c: &str) -> Grant {
        Grant::Venue { connection: c.into(), account_ref: None }
    }

    #[test]
    fn no_venue_grant_denies() {
        assert_eq!(
            check(&facts("alpaca-paper"), &[]),
            Err(Denied("venue not granted to this portfolio".into()))
        );
    }
    #[test]
    fn matching_venue_admits() {
        assert!(check(&facts("alpaca-paper"), &[venue("x"), venue("alpaca-paper")]).is_ok());
    }
    #[test]
    fn other_venue_denies() {
        assert_eq!(
            check(&facts("alpaca-paper"), &[venue("binance")]),
            Err(Denied("venue not granted to this portfolio".into()))
        );
    }
    #[test]
    fn serde_round_trip_venue() {
        let g = venue("alpaca-paper");
        let s = serde_json::to_string(&g).unwrap();
        assert_eq!(s, r#"{"kind":"venue","params":{"connection":"alpaca-paper","account_ref":null}}"#);
        assert_eq!(serde_json::from_str::<Grant>(&s).unwrap(), g);
    }
    #[test]
    fn unknown_kind_fails_to_parse() {
        assert!(serde_json::from_str::<Grant>(r#"{"kind":"nope","params":{}}"#).is_err());
    }
}
