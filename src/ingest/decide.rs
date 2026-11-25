//! Pure decision for one venue event: what the engine should do with it.
//! No DB, no async; the caller gathers the `Lookup` and acts on the `Decision`.

use gateway::venue_event::VenueEvent;
use uuid::Uuid;

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq)]
pub struct KnownOrder {
    pub order_id: Uuid,
    pub qty: f64,
    pub cum_qty: f64,
    pub external: bool,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub account_id: Option<Uuid>,
    pub client_id_prefix: Option<String>,
    pub portfolio_id: Uuid,
    pub priority: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Adopt,
    Break,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnPolicy {
    pub policy: Policy,
    pub default_portfolio: Option<Uuid>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lookup {
    /// Order found by parsing `client_order_id` as a UUID.
    pub by_client_uuid: Option<KnownOrder>,
    pub by_venue_id: Option<KnownOrder>,
    /// This fill's dedup key is already stored (fill-only).
    pub fill_seen: bool,
    /// An open break already exists for this venue order.
    pub open_break: bool,
    /// A leg execution of a multi-leg order (it names the parent) whose
    /// contract and side match none of the parent's leg orders (fill-only).
    /// The parent never books fills, so it cannot take it either.
    pub unmatched_leg: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakKind {
    PolicyBreak,
    NoBook,
    Overfill,
    UnknownInstrument,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Duplicate,
    Apply { order_id: Uuid },
    Grow { order_id: Uuid, new_qty: f64 },
    Adopt { portfolio_id: Uuid },
    AttachToBreak,
    Break(BreakKind),
    /// Not an order or fill event; nothing to decide here.
    Ignore,
}

/// Portfolio for an unknown order: first matching rule by ascending
/// `priority` (a rule matches when every field it sets matches), else the
/// connection default.
pub fn attribute(client_id: Option<&str>, account_id: Uuid, policy: &ConnPolicy) -> Option<Uuid> {
    let mut rules: Vec<&Rule> = policy.rules.iter().collect();
    rules.sort_by_key(|r| r.priority);
    rules
        .into_iter()
        .find(|r| {
            r.account_id.map_or(true, |a| a == account_id)
                && r.client_id_prefix
                    .as_deref()
                    .map_or(true, |p| client_id.is_some_and(|c| c.starts_with(p)))
        })
        .map(|r| r.portfolio_id)
        .or(policy.default_portfolio)
}

pub fn decide(event: &VenueEvent, account_id: Uuid, lookup: &Lookup, policy: &ConnPolicy) -> Decision {
    let (client_id, is_fill) = match event {
        VenueEvent::Fill(f) => (f.client_order_id.as_deref(), true),
        VenueEvent::Order(o) => (o.client_order_id.as_deref(), false),
        _ => return Decision::Ignore,
    };
    if is_fill && lookup.fill_seen {
        return Decision::Duplicate;
    }
    if lookup.open_break {
        return Decision::AttachToBreak;
    }
    if is_fill && lookup.unmatched_leg {
        return Decision::Break(BreakKind::UnknownInstrument);
    }
    if let Some(known) = lookup.by_client_uuid.as_ref().or(lookup.by_venue_id.as_ref()) {
        return match event {
            VenueEvent::Fill(f) => {
                let new_cum = known.cum_qty + f.qty;
                if new_cum > known.qty + EPS {
                    if known.external {
                        Decision::Grow { order_id: known.order_id, new_qty: new_cum }
                    } else {
                        Decision::Break(BreakKind::Overfill)
                    }
                } else {
                    Decision::Apply { order_id: known.order_id }
                }
            }
            VenueEvent::Order(o) if known.external && o.qty > known.qty + EPS => {
                Decision::Grow { order_id: known.order_id, new_qty: o.qty }
            }
            _ => Decision::Apply { order_id: known.order_id },
        };
    }
    if policy.policy == Policy::Break {
        return Decision::Break(BreakKind::PolicyBreak);
    }
    match attribute(client_id, account_id, policy) {
        Some(portfolio_id) => Decision::Adopt { portfolio_id },
        None => Decision::Break(BreakKind::NoBook),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::venue_event::{VenueFill, VenueOrderStatus, VenueOrderUpdate};

    fn fill(client: Option<&str>, qty: f64) -> VenueEvent {
        VenueEvent::Fill(VenueFill {
            exec_id: "e1".into(),
            venue_order_id: "v1".into(),
            client_order_id: client.map(str::to_string),
            account_ref: "acc".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            qty,
            price: 1.0,
            fee: None,
            executed_at: None,
        })
    }

    fn order(client: Option<&str>, qty: f64) -> VenueEvent {
        VenueEvent::Order(VenueOrderUpdate {
            venue_order_id: "v1".into(),
            client_order_id: client.map(str::to_string),
            account_ref: "acc".into(),
            symbol: "BTCUSDT".into(),
            buy: true,
            order_type: "LIMIT".into(),
            qty,
            limit_px: Some(1.0),
            status: VenueOrderStatus::Working,
            cum_qty: 0.0,
            ts: None,
            created_at: None,
        })
    }

    fn known(qty: f64, cum: f64, external: bool) -> KnownOrder {
        KnownOrder { order_id: Uuid::new_v4(), qty, cum_qty: cum, external, terminal: false }
    }

    fn lookup() -> Lookup {
        Lookup { by_client_uuid: None, by_venue_id: None, fill_seen: false, open_break: false, unmatched_leg: false }
    }

    fn adopt(default: Option<Uuid>, rules: Vec<Rule>) -> ConnPolicy {
        ConnPolicy { policy: Policy::Adopt, default_portfolio: default, rules }
    }

    fn rule(acc: Option<Uuid>, prefix: Option<&str>, pf: Uuid, priority: i32) -> Rule {
        Rule { account_id: acc, client_id_prefix: prefix.map(str::to_string), portfolio_id: pf, priority }
    }

    #[test]
    fn seen_fill_is_duplicate() {
        let l = Lookup { fill_seen: true, by_venue_id: Some(known(10.0, 0.0, false)), ..lookup() };
        assert_eq!(decide(&fill(None, 1.0), Uuid::new_v4(), &l, &adopt(None, vec![])), Decision::Duplicate);
    }

    #[test]
    fn a_leg_fill_matching_no_leg_order_is_an_unknown_instrument_break() {
        let l = Lookup { unmatched_leg: true, ..lookup() };
        let pol = adopt(Some(Uuid::new_v4()), vec![]);
        assert_eq!(decide(&fill(Some("p"), 1.0), Uuid::new_v4(), &l, &pol), Decision::Break(BreakKind::UnknownInstrument));
        // a redelivered fill is still a duplicate, and an open break still collects it
        assert_eq!(decide(&fill(None, 1.0), Uuid::new_v4(), &Lookup { fill_seen: true, ..l.clone() }, &pol), Decision::Duplicate);
        assert_eq!(decide(&fill(None, 1.0), Uuid::new_v4(), &Lookup { open_break: true, ..l }, &pol), Decision::AttachToBreak);
    }

    #[test]
    fn client_uuid_wins_over_venue_id() {
        let a = known(10.0, 0.0, false);
        let b = known(10.0, 0.0, false);
        let l = Lookup { by_client_uuid: Some(a.clone()), by_venue_id: Some(b), ..lookup() };
        assert_eq!(
            decide(&fill(None, 1.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Apply { order_id: a.order_id }
        );
    }

    #[test]
    fn fill_without_client_id_matches_by_venue_order_id() {
        let k = known(10.0, 0.0, false);
        let l = Lookup { by_venue_id: Some(k.clone()), ..lookup() };
        assert_eq!(
            decide(&fill(None, 1.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Apply { order_id: k.order_id }
        );
    }

    #[test]
    fn rule_by_prefix_beats_default() {
        let (p2, def) = (Uuid::new_v4(), Uuid::new_v4());
        let pol = adopt(Some(def), vec![rule(None, Some("p2-"), p2, 10)]);
        assert_eq!(
            decide(&fill(Some("p2-abc"), 1.0), Uuid::new_v4(), &lookup(), &pol),
            Decision::Adopt { portfolio_id: p2 }
        );
        assert_eq!(
            decide(&fill(Some("zzz"), 1.0), Uuid::new_v4(), &lookup(), &pol),
            Decision::Adopt { portfolio_id: def }
        );
    }

    #[test]
    fn lower_priority_number_wins_on_overlap() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let pol = adopt(None, vec![rule(None, Some("p"), b, 20), rule(None, Some("p2-"), a, 5)]);
        assert_eq!(
            decide(&fill(Some("p2-x"), 1.0), Uuid::new_v4(), &lookup(), &pol),
            Decision::Adopt { portfolio_id: a }
        );
    }

    #[test]
    fn account_rule_matches_only_that_account() {
        let (acc, pf) = (Uuid::new_v4(), Uuid::new_v4());
        let pol = adopt(None, vec![rule(Some(acc), None, pf, 1)]);
        assert_eq!(attribute(None, acc, &pol), Some(pf));
        assert_eq!(attribute(None, Uuid::new_v4(), &pol), None);
    }

    #[test]
    fn no_rule_no_default_is_break_nobook() {
        assert_eq!(
            decide(&fill(None, 1.0), Uuid::new_v4(), &lookup(), &adopt(None, vec![])),
            Decision::Break(BreakKind::NoBook)
        );
    }

    #[test]
    fn policy_break_breaks_even_with_default() {
        let pol = ConnPolicy { policy: Policy::Break, default_portfolio: Some(Uuid::new_v4()), rules: vec![] };
        assert_eq!(
            decide(&fill(None, 1.0), Uuid::new_v4(), &lookup(), &pol),
            Decision::Break(BreakKind::PolicyBreak)
        );
    }

    #[test]
    fn open_break_attaches() {
        let l = Lookup { open_break: true, ..lookup() };
        assert_eq!(
            decide(&fill(None, 1.0), Uuid::new_v4(), &l, &adopt(Some(Uuid::new_v4()), vec![])),
            Decision::AttachToBreak
        );
    }

    #[test]
    fn overfill_on_oms_order_is_a_break() {
        let l = Lookup { by_venue_id: Some(known(10.0, 8.0, false)), ..lookup() };
        assert_eq!(
            decide(&fill(None, 3.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Break(BreakKind::Overfill)
        );
    }

    #[test]
    fn overfill_on_external_order_grows() {
        let k = known(10.0, 8.0, true);
        let l = Lookup { by_venue_id: Some(k.clone()), ..lookup() };
        assert_eq!(
            decide(&fill(None, 3.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Grow { order_id: k.order_id, new_qty: 11.0 }
        );
    }

    #[test]
    fn order_update_grows_external_and_applies_to_oms() {
        let ext = known(10.0, 0.0, true);
        let l = Lookup { by_venue_id: Some(ext.clone()), ..lookup() };
        assert_eq!(
            decide(&order(None, 12.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Grow { order_id: ext.order_id, new_qty: 12.0 }
        );
        let oms = known(10.0, 0.0, false);
        let l = Lookup { by_venue_id: Some(oms.clone()), fill_seen: true, ..lookup() };
        assert_eq!(
            decide(&order(None, 12.0), Uuid::new_v4(), &l, &adopt(None, vec![])),
            Decision::Apply { order_id: oms.order_id }
        );
    }

    #[test]
    fn unknown_order_update_adopts() {
        let pf = Uuid::new_v4();
        assert_eq!(
            decide(&order(None, 1.0), Uuid::new_v4(), &lookup(), &adopt(Some(pf), vec![])),
            Decision::Adopt { portfolio_id: pf }
        );
    }

    #[test]
    fn non_order_events_are_ignored() {
        let l = lookup();
        let pol = adopt(None, vec![]);
        for e in [
            VenueEvent::Connected,
            VenueEvent::Disconnected,
            VenueEvent::Fee { exec_id: "e".into(), account_ref: "a".into(), amount: 1.0, currency: "USD".into() },
        ] {
            assert_eq!(decide(&e, Uuid::new_v4(), &l, &pol), Decision::Ignore);
        }
    }
}
