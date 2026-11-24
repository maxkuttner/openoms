//! Trading grants: what a portfolio is allowed to trade.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One thing a portfolio is allowed to do. Stored as `{"kind", "params"}`.
///
/// Future shapes: `InstrumentClass { connection, class }` and
/// `Hours { tz, days, from, to }`.
///
/// A new kind touches: (1) a variant here, (2) its arm in `kind()` and
/// `admits`, (3) `KINDS` and its arm in `spec` (default when absent + denial
/// message), (4) a UI section in the cockpit. The `every_variant_is_in_kinds`
/// test fails if (3) forgets `KINDS`.
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

/// Per-kind policy: the default when absent and the denial message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KindSpec {
    absent: Absent,
    denied: &'static str,
}

/// Every grant kind the check enforces.
const KINDS: &[&str] = &["venue"];

fn spec(kind: &str) -> KindSpec {
    match kind {
        "venue" => KindSpec { absent: Absent::Deny, denied: "venue not granted to this portfolio" },
        _ => KindSpec { absent: Absent::Allow, denied: "not granted to this portfolio" },
    }
}

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
        spec(kind).absent
    }

    fn admits(&self, f: &OrderFacts) -> bool {
        match self {
            // account_ref is a routing detail (which broker account to use),
            // not part of the permission check.
            Grant::Venue { connection, .. } => *connection == f.connection,
        }
    }
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
            return Err(Denied(spec(kind).denied.to_string()));
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
    #[test]
    fn every_variant_is_in_kinds() {
        let one_of_each = [venue("c")];
        for g in &one_of_each {
            assert!(KINDS.contains(&g.kind()), "{} missing from KINDS", g.kind());
        }
    }
    #[test]
    fn absent_defaults_are_pinned() {
        assert_eq!(Grant::when_absent("venue"), Absent::Deny);
        assert_eq!(Grant::when_absent("hours"), Absent::Allow);
        assert_eq!(spec("venue").denied, "venue not granted to this portfolio");
    }
    #[test]
    fn venue_with_account_ref_admits() {
        let g = Grant::Venue { connection: "ibkr".into(), account_ref: Some("DU1".into()) };
        assert!(check(&facts("ibkr"), &[g]).is_ok());
    }
    #[test]
    fn missing_or_malformed_params_fail_to_parse() {
        assert!(serde_json::from_str::<Grant>(r#"{"kind":"venue"}"#).is_err());
        assert!(serde_json::from_str::<Grant>(r#"{"kind":"venue","params":{}}"#).is_err());
        assert!(serde_json::from_str::<Grant>(r#"{"kind":"venue","params":{"connection":1}}"#).is_err());
    }
}
