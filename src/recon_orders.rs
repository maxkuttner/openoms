//! Transport-agnostic **order reconciliation** — snapshot-on-(re)connect.
//!
//! Functional core / imperative shell, mirroring [`crate::recon`] (which reconciles
//! *positions*). Here we reconcile *open orders*: fetch the broker's live order book
//! ([`BrokerAdapter::open_orders`]), diff it against the OMS working set, and heal
//! the drift the live execution stream may have missed while a session was down.
//!
//! Why this exists: neither transport guarantees no gaps. The Binance WS user-data
//! stream carries no sequence numbers and can silently drop events; the FIX sessions
//! run `ResetOnLogon=Y` with a memory store, which disables QuickFIX's own
//! resend/gap-fill. A full snapshot diff on every reconnect is the standard
//! mitigation, and it also surfaces orders that exist at the broker but not in the
//! OMS (out-of-band / manual).
//!
//! Safety: [`process_execution_report`] dedups only by the terminal-state skip (a
//! filled/canceled/rejected/expired order ignores further reports) under an
//! optimistic version lock — it has *no* per-fill idempotency. So we only ever
//! **apply** fills on the [`ReconAction::ResolveTerminal`] path, where the order is
//! closing and the terminal guard makes replay safe (a racing live report that
//! closes the order first simply causes our apply to be skipped). Partial drift on a
//! *still-open* order is reported ([`ReconAction::PartialDrift`]) but not auto-applied
//! — applying a bare delta there could double-count a concurrent live fill. It heals
//! for free when the order eventually resolves (ResolveTerminal reconciles the full
//! executed quantity at close).

use std::collections::{HashMap, HashSet};

use sqlx::{PgPool, Row};
use tracing::{error, info, warn};
use uuid::Uuid;

use gateway::adapter::{BrokerAdapter, BrokerError};
use gateway::contract::{BrokerOpenOrder, BrokerOrderState, EngineHandle, ExecEvent, ExecKind, ExecSource};

const EPS: f64 = 1e-9;

/// One OMS order the broker still owns (status `routed` / `partially_filled`), with
/// the fills we've recorded so far.
#[derive(Debug, Clone, PartialEq)]
pub struct OmsWorkingOrder {
    pub order_id: Uuid,
    pub symbol: String,
    /// The broker's own order id (from the `order_routed` event) — how we ask the
    /// broker about this order.
    pub ext_id: String,
    pub cum_qty: f64,
    /// Average price of what the OMS has booked so far (`None` before any fill),
    /// against which a missed quantity is priced.
    pub avg_px: Option<f64>,
}

/// What the diff decided to do about one order.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconAction {
    /// OMS thinks it's working but the broker no longer lists it open — resolve its
    /// terminal state and apply what we missed. Safe to apply (terminal guard).
    ResolveTerminal { order_id: Uuid, symbol: String, ext_id: String, oms_cum: f64 },
    /// Broker `executed_qty` is ahead of OMS `cum_qty` on a still-open order — report
    /// only (see module docs on why we don't auto-apply here).
    PartialDrift { order_id: Uuid, oms_cum: f64, broker_executed: f64 },
    /// The broker lists an order the OMS doesn't track (out-of-band / manual / a
    /// client id we never issued) — surface it.
    OrphanAtBroker { client_order_id: String, symbol: String },
}

/// Pure diff: OMS working set vs the broker's open-order snapshot. Keyed on
/// `client_order_id == order_id`.
pub fn plan_order_recon(oms: &[OmsWorkingOrder], broker_open: &[BrokerOpenOrder]) -> Vec<ReconAction> {
    let mut actions = Vec::new();

    let open: HashMap<&str, &BrokerOpenOrder> =
        broker_open.iter().map(|o| (o.client_order_id.as_str(), o)).collect();

    for o in oms {
        let id = o.order_id.to_string();
        match open.get(id.as_str()) {
            // Gone from the broker's open book → it resolved while we weren't looking.
            None => actions.push(ReconAction::ResolveTerminal {
                order_id: o.order_id,
                symbol: o.symbol.clone(),
                ext_id: o.ext_id.clone(),
                oms_cum: o.cum_qty,
            }),
            // Still open, but the broker has filled more than we've recorded.
            Some(b) if b.executed_qty - o.cum_qty > EPS => {
                actions.push(ReconAction::PartialDrift {
                    order_id: o.order_id,
                    oms_cum: o.cum_qty,
                    broker_executed: b.executed_qty,
                });
            }
            Some(_) => {}
        }
    }

    // Broker-side orders the OMS doesn't know about at all.
    let oms_ids: HashSet<String> = oms.iter().map(|o| o.order_id.to_string()).collect();
    for b in broker_open {
        if !oms_ids.contains(&b.client_order_id) {
            actions.push(ReconAction::OrphanAtBroker {
                client_order_id: b.client_order_id.clone(),
                symbol: b.symbol.clone(),
            });
        }
    }

    actions
}

/// Load the OMS orders the broker still owns for `broker_code` (e.g. `"BINANCE"` /
/// `"IBKR"`): status `routed` or `partially_filled`, resolved to their broker symbol
/// and external order id. Orders without a routed external id are skipped (nothing to
/// ask the broker about).
pub async fn load_oms_working_orders(pool: &PgPool, broker_code: &str, environment: &str) -> Vec<OmsWorkingOrder> {
    // One round-trip: join the broker symbol and pull the routed external id inline,
    // rather than two per-order lookups (this runs on every reconnect + a 30s heartbeat).
    let rows = match sqlx::query(
        "SELECT os.order_id, bi.broker_symbol AS symbol, os.cum_qty::float8 AS cum_qty, \
                os.avg_px::float8 AS avg_px, \
                (SELECT payload_json->'payload'->>'external_order_id' \
                   FROM order_event \
                  WHERE order_id = os.order_id AND event_type = 'order_routed' \
                  LIMIT 1) AS ext_id \
         FROM order_state os \
         JOIN broker_connection bc ON bc.code = os.broker_connection_code \
         JOIN broker_instrument bi \
           ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = $1 \
         WHERE os.status IN ('routed', 'partially_filled') AND bc.broker_code = $1 AND bc.environment = $2",
    )
    .bind(broker_code)
    .bind(environment)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!(broker = broker_code, error = %e, "order recon: working-order query failed");
            return Vec::new();
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // Skip orders with no routed external id — nothing to ask the broker about.
        let Some(ext_id) = row.get::<Option<String>, _>("ext_id").filter(|s| !s.is_empty()) else {
            continue;
        };
        out.push(OmsWorkingOrder {
            order_id: row.get("order_id"),
            symbol: row.get("symbol"),
            ext_id,
            cum_qty: row.get("cum_qty"),
            avg_px: row.get("avg_px"),
        });
    }
    out
}

/// The imperative shell: read the OMS working set, then reconcile it against the
/// broker. `broker_code` is the uppercase registry/venue code (`"BINANCE"`);
/// `actor` labels the events booked from it (`"binance"` / `"ibkr"`).
pub async fn run_order_reconcile(
    pool: &PgPool,
    adapter: &dyn BrokerAdapter,
    broker_code: &'static str,
    environment: &'static str,
    actor: &'static str,
    engine: &EngineHandle,
) {
    let oms = load_oms_working_orders(pool, broker_code, environment).await;
    let stuck = load_stuck_orders(pool, broker_code, environment).await;
    reconcile_with(&oms, &stuck, adapter, broker_code, actor, engine).await;
}

/// How long an order may stay `submitted` before recon asks the venue about it:
/// routing normally records the venue's id within a second or two.
pub const STUCK_AFTER_SECS: u64 = 60;

/// Orders left `submitted` past the grace period on one connection (`$1` broker
/// code, `$2` environment): routing failed to record the venue's answer.
pub const STUCK_ORDERS_SQL: &str = "SELECT os.order_id, bi.broker_symbol AS symbol \
     FROM order_state os \
     JOIN broker_connection bc ON bc.code = os.broker_connection_code \
     JOIN broker_instrument bi ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = $1 \
     WHERE os.status = 'submitted' AND bc.broker_code = $1 AND bc.environment = $2 \
       AND os.created_at < now() - interval '60 seconds'";

/// An order stuck in `submitted`.
#[derive(Debug, Clone, PartialEq)]
pub struct StuckOrder {
    pub order_id: Uuid,
    pub symbol: String,
}

pub async fn load_stuck_orders(pool: &PgPool, broker_code: &str, environment: &str) -> Vec<StuckOrder> {
    match sqlx::query_as::<_, (Uuid, String)>(STUCK_ORDERS_SQL).bind(broker_code).bind(environment).fetch_all(pool).await {
        Ok(rows) => rows.into_iter().map(|(order_id, symbol)| StuckOrder { order_id, symbol }).collect(),
        Err(e) => {
            warn!(broker = broker_code, error = %e, "order recon: could not load stuck orders");
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StuckAction {
    /// The venue works it: record its id (fills heal as for any routed order).
    Route { order_id: Uuid, venue_order_id: String },
    /// Not open at the venue: ask how it ended, or whether it ever arrived.
    AskStatus { order_id: Uuid, symbol: String },
}

pub fn plan_stuck(stuck: &[StuckOrder], broker_open: &[BrokerOpenOrder]) -> Vec<StuckAction> {
    stuck
        .iter()
        .map(|s| match broker_open.iter().find(|b| b.client_order_id == s.order_id.to_string()) {
            Some(b) => StuckAction::Route { order_id: s.order_id, venue_order_id: b.venue_order_id.clone() },
            None => StuckAction::AskStatus { order_id: s.order_id, symbol: s.symbol.clone() },
        })
        .collect()
}

/// What a stuck order's status answer books; `None` leaves it as it is.
pub fn stuck_outcome(answer: &Result<BrokerOrderState, BrokerError>) -> Option<ExecKind> {
    match answer {
        Ok(state) => Some(ExecKind::Terminal(state.clone())),
        Err(BrokerError::NoSuchOrder(_)) => Some(ExecKind::Rejected { reason: "never reached the broker".into() }),
        Err(_) => None,
    }
}

/// Snapshot the broker's open orders, diff against `oms`, and act on each
/// difference. Recon never books: an order that closed at the broker goes to the
/// order engine as a terminal state, and the engine books it against what is
/// booked *then* — in arrival order with live fills — not against `oms`, which a
/// live fill may have overtaken while recon waited on the broker.
pub async fn reconcile_with(
    oms: &[OmsWorkingOrder],
    stuck: &[StuckOrder],
    adapter: &dyn BrokerAdapter,
    broker_code: &'static str,
    actor: &'static str,
    engine: &EngineHandle,
) {
    let broker_open = match adapter.open_orders().await {
        Ok(o) => o,
        Err(BrokerError::NotConfigured(_)) => return, // adapter doesn't support snapshots
        Err(e) => {
            warn!(broker = broker_code, error = %e, "order recon: open-orders snapshot failed");
            return;
        }
    };

    if oms.is_empty() && broker_open.is_empty() && stuck.is_empty() {
        return;
    }

    for action in plan_stuck(stuck, &broker_open) {
        let (order_id, kind) = match action {
            StuckAction::Route { order_id, venue_order_id } => {
                info!(order_id = %order_id, %venue_order_id, "order recon: a stuck order is open at the broker; routing it");
                (order_id, ExecKind::Ack { venue_order_id })
            }
            StuckAction::AskStatus { order_id, symbol } => {
                let answer = adapter.order_status(&order_id.to_string(), "", &symbol).await;
                match stuck_outcome(&answer) {
                    Some(kind) => (order_id, kind),
                    None => {
                        error!(order_id = %order_id, answer = ?answer.err(), "order recon: cannot tell what became of a stuck order; left submitted");
                        continue;
                    }
                }
            }
        };
        engine.exec(ExecEvent { order_id, venue: broker_code.to_string(), actor, source: ExecSource::Snapshot, kind }).await;
    }

    // A stuck order is settled above, not reported as an orphan.
    let broker_open: Vec<BrokerOpenOrder> = broker_open
        .into_iter()
        .filter(|b| !stuck.iter().any(|s| s.order_id.to_string() == b.client_order_id))
        .collect();

    for action in plan_order_recon(oms, &broker_open) {
        match action {
            ReconAction::ResolveTerminal { order_id, symbol, ext_id, .. } => {
                let st = match adapter.order_status(&order_id.to_string(), &ext_id, &symbol).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(order_id = %order_id, error = %e, "order recon: order_status failed");
                        continue;
                    }
                };
                engine
                    .exec(ExecEvent {
                        order_id,
                        venue: broker_code.to_string(),
                        actor,
                        source: ExecSource::Snapshot,
                        kind: ExecKind::Terminal(st),
                    })
                    .await;
            }
            ReconAction::PartialDrift { order_id, oms_cum, broker_executed } => {
                // Report only — healed safely at close. See module docs.
                warn!(
                    order_id = %order_id, oms_cum, broker_executed,
                    "order recon: partial-fill drift (broker ahead of OMS); will heal at close"
                );
            }
            ReconAction::OrphanAtBroker { client_order_id, symbol } => {
                warn!(
                    broker = broker_code, client_order_id, symbol,
                    "order recon: broker reports an open order the OMS doesn't track"
                );
            }
        }
    }
}

/// How much of `broker_cum` the OMS hasn't booked yet, or `None` when it is
/// already booked (a repeated, replayed or older update).
pub(crate) fn missing_qty(oms_cum: f64, broker_cum: f64) -> Option<f64> {
    let missing = broker_cum - oms_cum;
    (missing > EPS).then_some(missing)
}

/// The price of the quantity the broker has filled beyond what the OMS booked,
/// from the two cumulative states: `(avg_b·cum_b − avg_oms·cum_oms) / (cum_b −
/// cum_oms)`. Falls back to the broker's average when nothing is booked, no
/// booked price is known, nothing is missing, or the result isn't a positive price.
pub(crate) fn increment_price(oms_cum: f64, oms_avg: Option<f64>, broker_cum: f64, broker_avg: f64) -> f64 {
    let missing = broker_cum - oms_cum;
    match oms_avg {
        Some(oms_avg) if oms_cum > EPS && missing > EPS => {
            let price = (broker_avg * broker_cum - oms_avg * oms_cum) / missing;
            if price.is_finite() && price > 0.0 { price } else { broker_avg }
        }
        _ => broker_avg,
    }
}

/// The OMS's own booked state for one order — cumulative quantity and average
/// price; `None` (logged) when the order is unknown or unreadable.
pub(crate) async fn oms_fill_state(pool: &PgPool, order_id: Uuid) -> Option<(f64, Option<f64>)> {
    match sqlx::query_as::<_, (f64, Option<f64>)>(
        "SELECT cum_qty::float8, avg_px::float8 FROM order_state WHERE order_id = $1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(state)) => Some(state),
        Ok(None) => {
            warn!(order_id = %order_id, "broker update for an order the OMS doesn't have");
            None
        }
        Err(e) => {
            error!(order_id = %order_id, error = %e, "broker update: could not read the order's fills");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::contract::{BrokerOrderOutcome, BrokerOrderState};

    // Cumulative venues (Coinbase, OKX) book "venue cumulative − OMS cum_qty", so
    // a repeated or replayed update (same cumulative) books nothing.
    #[test]
    fn missing_qty_books_only_what_is_new() {
        assert_eq!(missing_qty(0.0, 0.0005), Some(0.0005));
        assert_eq!(missing_qty(0.0005, 0.0005), None);
        assert_eq!(missing_qty(0.001, 0.0005), None, "an older update after a newer one");
    }

    /// A broker whose snapshot no longer lists the order, and which reports it
    /// canceled with some quantity executed.
    struct ClosedAtBroker;

    #[async_trait::async_trait]
    impl BrokerAdapter for ClosedAtBroker {
        async fn submit_order(
            &self,
            _: &gateway::adapter::BrokerOrderRequest,
        ) -> Result<gateway::adapter::BrokerOrderResponse, BrokerError> {
            unreachable!()
        }
        async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> {
            unreachable!()
        }
        async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
            Ok(Vec::new())
        }
        async fn order_status(&self, _: &str, _: &str, _: &str) -> Result<BrokerOrderState, BrokerError> {
            Ok(BrokerOrderState { outcome: BrokerOrderOutcome::Canceled, executed_qty: 0.4, avg_px: 10.0 })
        }
    }

    /// Has no open orders and no order by any id.
    struct NeverSaw;

    #[async_trait::async_trait]
    impl BrokerAdapter for NeverSaw {
        async fn submit_order(&self, _: &gateway::adapter::BrokerOrderRequest) -> Result<gateway::adapter::BrokerOrderResponse, BrokerError> {
            unimplemented!()
        }
        async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> {
            unimplemented!()
        }
        async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
            Ok(Vec::new())
        }
        async fn order_status(&self, client: &str, external: &str, _: &str) -> Result<BrokerOrderState, BrokerError> {
            assert!(external.is_empty(), "a stuck order is asked about by our id");
            Err(BrokerError::NoSuchOrder(client.to_string()))
        }
    }

    #[tokio::test]
    async fn recon_rejects_a_stuck_order_the_venue_never_saw() {
        let order_id = Uuid::new_v4();
        let (engine, mut rx) = gateway::contract::EngineHandle::new();
        reconcile_with(&[], &[stuck(order_id)], &NeverSaw, "FAKE", "fake", &engine).await;
        let Ok(gateway::contract::GatewayMsg::Exec(ev)) = rx.try_recv() else { panic!("an exec event") };
        assert_eq!(ev.order_id, order_id);
        assert_eq!(ev.kind, ExecKind::Rejected { reason: "never reached the broker".into() });
    }

    /// Recon only asks the broker; the order engine books, against what is
    /// booked when it gets there — never against the snapshot recon read at its
    /// start, which a live fill may have overtaken.
    #[tokio::test]
    async fn recon_hands_terminal_states_to_the_engine_instead_of_booking() {
        let order_id = Uuid::new_v4();
        let oms = [OmsWorkingOrder { order_id, symbol: "X".into(), ext_id: "e1".into(), cum_qty: 0.0, avg_px: None }];
        let (engine, mut rx) = gateway::contract::EngineHandle::new();

        reconcile_with(&oms, &[], &ClosedAtBroker, "FAKE", "fake", &engine).await;

        let Ok(gateway::contract::GatewayMsg::Exec(ev)) = rx.try_recv() else { panic!("an exec event") };
        assert_eq!(ev.order_id, order_id);
        assert_eq!(ev.source, ExecSource::Snapshot);
        assert_eq!(
            ev.kind,
            ExecKind::Terminal(BrokerOrderState { outcome: BrokerOrderOutcome::Canceled, executed_qty: 0.4, avg_px: 10.0 })
        );
    }

    // The missing quantity is priced from the OMS's own booked state, not from
    // whatever a connection happened to see: 0.5 booked @100, Coinbase now says
    // 1.0 @105 average, so the missing 0.5 filled @110.
    #[test]
    fn the_missing_quantity_is_priced_against_what_the_oms_booked() {
        assert!((increment_price(0.5, Some(100.0), 1.0, 105.0) - 110.0).abs() < 1e-9);
        assert_eq!(increment_price(0.0, None, 1.0, 105.0), 105.0, "nothing booked yet: the broker's average");
        assert_eq!(increment_price(0.5, None, 1.0, 105.0), 105.0, "no booked price known: the broker's average");
        assert_eq!(increment_price(1.0, Some(100.0), 1.0, 105.0), 105.0, "no missing quantity");
        assert_eq!(increment_price(0.5, Some(300.0), 1.0, 105.0), 105.0, "a negative increment price falls back");
    }

    fn oms(id: Uuid, cum: f64) -> OmsWorkingOrder {
        OmsWorkingOrder { order_id: id, symbol: "BTCUSDT".into(), ext_id: "9".into(), cum_qty: cum, avg_px: None }
    }
    fn open(id: &str, executed: f64) -> BrokerOpenOrder {
        BrokerOpenOrder { client_order_id: id.into(), venue_order_id: format!("v-{id}"), symbol: "BTCUSDT".into(), executed_qty: executed }
    }

    fn stuck(id: Uuid) -> StuckOrder {
        StuckOrder { order_id: id, symbol: "BTCUSDT".into() }
    }

    #[test]
    fn a_stuck_order_open_at_the_broker_is_routed() {
        let id = Uuid::new_v4();
        let actions = plan_stuck(&[stuck(id)], &[open(&id.to_string(), 0.5)]);
        assert_eq!(actions, vec![StuckAction::Route { order_id: id, venue_order_id: format!("v-{id}") }]);
    }

    #[test]
    fn a_stuck_order_absent_at_the_broker_is_asked_about() {
        let id = Uuid::new_v4();
        assert_eq!(plan_stuck(&[stuck(id)], &[]), vec![StuckAction::AskStatus { order_id: id, symbol: "BTCUSDT".into() }]);
    }

    #[test]
    fn a_young_submitted_order_is_left_alone() {
        assert!(STUCK_ORDERS_SQL.contains(&format!("interval '{STUCK_AFTER_SECS} seconds'")));
        assert_eq!(STUCK_AFTER_SECS, 60);
    }

    #[test]
    fn an_unknown_outcome_leaves_the_order() {
        assert!(stuck_outcome(&Err(BrokerError::NotConfigured("no lookup".into()))).is_none());
        assert!(stuck_outcome(&Err(BrokerError::Network("timeout".into()))).is_none());
        assert!(matches!(
            stuck_outcome(&Err(BrokerError::NoSuchOrder("x".into()))),
            Some(ExecKind::Rejected { reason }) if reason == "never reached the broker"
        ));
        let st = BrokerOrderState { outcome: gateway::contract::BrokerOrderOutcome::Filled, executed_qty: 1.0, avg_px: 2.0 };
        assert!(matches!(stuck_outcome(&Ok(st)), Some(ExecKind::Terminal(_))));
    }

    #[test]
    fn clean_when_open_sets_match() {
        let id = Uuid::new_v4();
        let actions = plan_order_recon(&[oms(id, 0.0)], &[open(&id.to_string(), 0.0)]);
        assert!(actions.is_empty());
    }

    #[test]
    fn missed_terminal_when_absent_from_broker_open() {
        let id = Uuid::new_v4();
        let actions = plan_order_recon(&[oms(id, 0.0)], &[]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], ReconAction::ResolveTerminal { order_id, .. } if order_id == id));
    }

    #[test]
    fn missed_partial_when_broker_ahead() {
        let id = Uuid::new_v4();
        let actions = plan_order_recon(&[oms(id, 1.0)], &[open(&id.to_string(), 3.0)]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            ReconAction::PartialDrift { order_id, oms_cum, broker_executed }
                if order_id == id && oms_cum == 1.0 && broker_executed == 3.0
        ));
    }

    #[test]
    fn no_drift_when_broker_equals_oms() {
        let id = Uuid::new_v4();
        let actions = plan_order_recon(&[oms(id, 2.0)], &[open(&id.to_string(), 2.0)]);
        assert!(actions.is_empty());
    }

    #[test]
    fn orphan_when_broker_has_untracked_order() {
        let actions = plan_order_recon(&[], &[open("not-ours", 1.0)]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ReconAction::OrphanAtBroker { client_order_id, .. } if client_order_id == "not-ours"));
    }
}
