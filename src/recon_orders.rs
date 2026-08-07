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
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::adapters::{BrokerAdapter, BrokerError};
use crate::domain::orders::commands::ExecutionReport;
use crate::execution::process_execution_report;
use crate::kafka::KafkaClient;

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
}

/// One order the broker currently reports as open, normalized across transports.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerOpenOrder {
    /// Our order UUID (the client order id we sent).
    pub client_order_id: String,
    pub symbol: String,
    pub executed_qty: f64,
}

/// How the broker says a no-longer-open order finished (from `order_status`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BrokerOrderOutcome {
    Filled,
    Canceled,
    Rejected,
    /// Still open at the broker (shouldn't happen on the ResolveTerminal path).
    Working,
}

/// One order's current state at the broker, resolved on demand for an OMS order that
/// dropped out of the open-order snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct BrokerOrderState {
    pub outcome: BrokerOrderOutcome,
    pub executed_qty: f64,
    pub avg_px: f64,
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
pub async fn load_oms_working_orders(pool: &PgPool, broker_code: &str) -> Vec<OmsWorkingOrder> {
    // One round-trip: join the broker symbol and pull the routed external id inline,
    // rather than two per-order lookups (this runs on every reconnect + a 30s heartbeat).
    let rows = match sqlx::query(
        "SELECT os.order_id, bi.broker_symbol AS symbol, os.cum_qty::float8 AS cum_qty, \
                (SELECT payload_json->'payload'->>'external_order_id' \
                   FROM order_event \
                  WHERE order_id = os.order_id AND event_type = 'order_routed' \
                  LIMIT 1) AS ext_id \
         FROM order_state os \
         JOIN broker_connection bc ON bc.code = os.broker_connection_code \
         JOIN broker_instrument bi \
           ON bi.instrument_id = os.instrument_id::bigint AND bi.broker_code = $1 \
         WHERE os.status IN ('routed', 'partially_filled') AND bc.broker_code = $1",
    )
    .bind(broker_code)
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
        });
    }
    out
}

/// The imperative shell: snapshot the broker's open orders, diff against the OMS
/// working set, and act on each difference. Used by both the Binance WS driver and
/// the FIX recon driver. `broker_code` is the uppercase registry/venue code
/// (`"BINANCE"`); `actor` labels appended events (`"binance"` / `"ibkr"`).
pub async fn run_order_reconcile(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    adapter: &dyn BrokerAdapter,
    broker_code: &str,
    actor: &str,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) {
    let oms = load_oms_working_orders(pool, broker_code).await;

    let broker_open = match adapter.open_orders().await {
        Ok(o) => o,
        Err(BrokerError::NotConfigured(_)) => return, // adapter doesn't support snapshots
        Err(e) => {
            warn!(broker = broker_code, error = %e, "order recon: open-orders snapshot failed");
            return;
        }
    };

    if oms.is_empty() && broker_open.is_empty() {
        return;
    }

    for action in plan_order_recon(&oms, &broker_open) {
        match action {
            ReconAction::ResolveTerminal { order_id, symbol, ext_id, oms_cum } => {
                let st = match adapter.order_status(&order_id.to_string(), &ext_id, &symbol).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(order_id = %order_id, error = %e, "order recon: order_status failed");
                        continue;
                    }
                };
                apply_terminal(pool, kafka, order_id, oms_cum, &st, broker_code, actor, position_changed_tx)
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

/// Apply the terminal outcome of an order that dropped out of the open-order set.
/// Emits the missed fill (full executed − OMS cum) first, then the close, so a
/// partial-then-canceled order books its trade before releasing the remainder. The
/// terminal-state guard in [`process_execution_report`] makes every step replay-safe.
async fn apply_terminal(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    order_id: Uuid,
    oms_cum: f64,
    st: &BrokerOrderState,
    venue: &str,
    actor: &str,
    position_changed_tx: Option<&mpsc::Sender<()>>,
) {
    let apply = |report: ExecutionReport| async move {
        if let Err(e) =
            process_execution_report(pool, kafka, order_id, report, actor, position_changed_tx).await
        {
            error!(order_id = %order_id, error = %e, "order recon: apply failed");
        }
    };

    let missed = st.executed_qty - oms_cum;
    if missed > EPS {
        apply(ExecutionReport::Fill {
            execution_id: format!("recon-{order_id}"),
            fill_qty: missed,
            fill_price: st.avg_px,
            venue: venue.to_string(),
        })
        .await;
    }

    match st.outcome {
        BrokerOrderOutcome::Filled => {
            // The fill above closes it (leaves → 0). Nothing else to do.
        }
        BrokerOrderOutcome::Canceled => {
            apply(ExecutionReport::Canceled { reason: None, venue: Some(venue.to_string()) }).await;
        }
        BrokerOrderOutcome::Rejected => {
            apply(ExecutionReport::Reject {
                reason: "rejected".to_string(),
                venue: Some(venue.to_string()),
            })
            .await;
        }
        BrokerOrderOutcome::Working => {
            info!(order_id = %order_id, "order recon: order still working at broker, no terminal action");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oms(id: Uuid, cum: f64) -> OmsWorkingOrder {
        OmsWorkingOrder { order_id: id, symbol: "BTCUSDT".into(), ext_id: "9".into(), cum_qty: cum }
    }
    fn open(id: &str, executed: f64) -> BrokerOpenOrder {
        BrokerOpenOrder { client_order_id: id.into(), symbol: "BTCUSDT".into(), executed_qty: executed }
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
