//! Custodian reconciliation (read & compare).
//!
//! Functional core / imperative shell: `reconcile` is a pure diff of OMS positions
//! vs the custodian's holdings; `run_reconciliation` sources both sides (Postgres +
//! the broker/custodian adapter), resolves symbology, and persists the result.
//!
//! v1 custodian = Alpaca paper (broker == custodian). Holdings resolve to our master
//! instrument through the existing `broker_instrument` bridge.

use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::adapters::BrokerRegistry;

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BreakKind {
    /// Both sides hold it, quantities differ.
    QtyMismatch,
    /// OMS holds it, the custodian doesn't.
    MissingInCustodian,
    /// Custodian holds it, the OMS doesn't.
    MissingInOms,
    /// Custodian reported a security we couldn't map to a master instrument.
    UnresolvedCustodianSecurity,
}

impl BreakKind {
    fn as_str(self) -> &'static str {
        match self {
            BreakKind::QtyMismatch => "qty_mismatch",
            BreakKind::MissingInCustodian => "missing_in_custodian",
            BreakKind::MissingInOms => "missing_in_oms",
            BreakKind::UnresolvedCustodianSecurity => "unresolved_custodian_security",
        }
    }
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ReconBreak {
    pub instrument_id: Option<String>,
    pub symbol: Option<String>,
    pub oms_qty: f64,
    pub custodian_qty: f64,
    pub diff: f64,
    pub kind: BreakKind,
}

/// A custodian holding after symbology resolution. `instrument_id == None` means the
/// custodian's security didn't map to any master instrument.
pub struct ResolvedHolding {
    pub instrument_id: Option<String>,
    pub symbol: String,
    pub qty: f64,
}

/// Pure diff. `oms` is (instrument_id, signed qty); `custodian` is resolved holdings.
pub fn reconcile(oms: &[(String, f64)], custodian: &[ResolvedHolding]) -> Vec<ReconBreak> {
    use std::collections::HashMap;
    let mut breaks = Vec::new();

    // Accumulate resolved custodian qty per instrument; flag unresolved immediately.
    let mut cust: HashMap<String, (f64, String)> = HashMap::new();
    for h in custodian {
        match &h.instrument_id {
            None => breaks.push(ReconBreak {
                instrument_id: None,
                symbol: Some(h.symbol.clone()),
                oms_qty: 0.0,
                custodian_qty: h.qty,
                diff: h.qty,
                kind: BreakKind::UnresolvedCustodianSecurity,
            }),
            Some(id) => {
                let e = cust.entry(id.clone()).or_insert((0.0, h.symbol.clone()));
                e.0 += h.qty;
            }
        }
    }

    let oms_map: HashMap<&str, f64> = oms.iter().map(|(k, v)| (k.as_str(), *v)).collect();

    // OMS-side rows: mismatch or missing-in-custodian.
    for (id, &oms_qty) in &oms_map {
        let cust_entry = cust.get(*id);
        let cust_qty = cust_entry.map(|(q, _)| *q).unwrap_or(0.0);
        let diff = oms_qty - cust_qty;
        if diff.abs() > EPS {
            breaks.push(ReconBreak {
                instrument_id: Some((*id).to_string()),
                symbol: cust_entry.map(|(_, s)| s.clone()),
                oms_qty,
                custodian_qty: cust_qty,
                diff,
                kind: if cust_entry.is_some() {
                    BreakKind::QtyMismatch
                } else {
                    BreakKind::MissingInCustodian
                },
            });
        }
    }

    // Custodian-side rows the OMS doesn't have at all.
    for (id, (cust_qty, symbol)) in &cust {
        if !oms_map.contains_key(id.as_str()) && cust_qty.abs() > EPS {
            breaks.push(ReconBreak {
                instrument_id: Some(id.clone()),
                symbol: Some(symbol.clone()),
                oms_qty: 0.0,
                custodian_qty: *cust_qty,
                diff: -*cust_qty,
                kind: BreakKind::MissingInOms,
            });
        }
    }

    breaks
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReconSummary {
    pub run_id: Uuid,
    pub broker_connection_code: String,
    pub oms_count: usize,
    pub custodian_count: usize,
    pub break_count: usize,
    pub breaks: Vec<ReconBreak>,
}

#[derive(Debug)]
pub enum ReconError {
    NotFound(String),
    Unsupported(String),
    NoAdapter(String),
    Broker(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ReconError {
    fn from(e: sqlx::Error) -> Self {
        ReconError::Db(e)
    }
}

/// Source both sides for one broker connection, diff, and persist the run + breaks.
pub async fn run_reconciliation(
    pool: &PgPool,
    registry: &BrokerRegistry,
    broker_connection_code: &str,
) -> Result<ReconSummary, ReconError> {
    let conn = sqlx::query("SELECT broker_code, environment FROM broker_connection WHERE code = $1")
        .bind(broker_connection_code)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ReconError::NotFound(format!("broker connection {broker_connection_code}")))?;
    let broker_code: String = conn.get("broker_code");
    let environment: String = conn.get("environment");

    // v1 supports Alpaca (broker == custodian). Other brokers need their own holdings source.
    if broker_code != "ALPACA" {
        return Err(ReconError::Unsupported(broker_code));
    }
    let adapter = registry
        .get_alpaca(&environment)
        .ok_or_else(|| ReconError::NoAdapter(format!("ALPACA/{environment}")))?;
    let holdings = adapter
        .get_positions()
        .await
        .map_err(|e| ReconError::Broker(e.to_string()))?;

    // Custodian side: resolve each holding to a master instrument via broker_instrument.
    let mut resolved = Vec::with_capacity(holdings.len());
    for h in &holdings {
        let instrument_id: Option<i64> = sqlx::query_scalar(
            "SELECT instrument_id FROM broker_instrument \
             WHERE broker_code = $1 AND (native_id = $2 OR broker_symbol = $3) \
             ORDER BY (native_id = $2) DESC NULLS LAST LIMIT 1",
        )
        .bind(&broker_code)
        .bind(&h.native_id)
        .bind(&h.symbol)
        .fetch_optional(pool)
        .await?;
        resolved.push(ResolvedHolding {
            instrument_id: instrument_id.map(|i| i.to_string()),
            symbol: h.symbol.clone(),
            qty: h.qty,
        });
    }

    // OMS side: net position per instrument across portfolios routing to this connection.
    let oms_rows = sqlx::query(
        "SELECT po.instrument_id, SUM(po.net_qty)::float8 AS qty \
         FROM position po \
         JOIN portfolio pf ON pf.id = po.portfolio_id \
         JOIN account a ON a.id = pf.default_account_id \
         WHERE a.broker_connection_code = $1 \
         GROUP BY po.instrument_id \
         HAVING SUM(po.net_qty) <> 0",
    )
    .bind(broker_connection_code)
    .fetch_all(pool)
    .await?;
    let oms: Vec<(String, f64)> = oms_rows
        .iter()
        .map(|r| (r.get::<String, _>("instrument_id"), r.get::<f64, _>("qty")))
        .collect();

    let breaks = reconcile(&oms, &resolved);

    // Persist the run and its breaks.
    let run_id: Uuid = sqlx::query_scalar(
        "INSERT INTO recon_run \
           (broker_connection_code, oms_count, custodian_count, break_count, finished_at) \
         VALUES ($1, $2, $3, $4, now()) RETURNING id",
    )
    .bind(broker_connection_code)
    .bind(oms.len() as i32)
    .bind(resolved.len() as i32)
    .bind(breaks.len() as i32)
    .fetch_one(pool)
    .await?;

    for b in &breaks {
        sqlx::query(
            "INSERT INTO recon_break \
               (recon_run_id, instrument_id, symbol, oms_qty, custodian_qty, diff, kind) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(run_id)
        .bind(&b.instrument_id)
        .bind(&b.symbol)
        .bind(b.oms_qty)
        .bind(b.custodian_qty)
        .bind(b.diff)
        .bind(b.kind.as_str())
        .execute(pool)
        .await?;
    }

    Ok(ReconSummary {
        run_id,
        broker_connection_code: broker_connection_code.to_string(),
        oms_count: oms.len(),
        custodian_count: resolved.len(),
        break_count: breaks.len(),
        breaks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: Option<&str>, symbol: &str, qty: f64) -> ResolvedHolding {
        ResolvedHolding {
            instrument_id: id.map(|s| s.to_string()),
            symbol: symbol.to_string(),
            qty,
        }
    }

    #[test]
    fn clean_when_sides_match() {
        let breaks = reconcile(&[("1".into(), 100.0)], &[h(Some("1"), "SPY", 100.0)]);
        assert!(breaks.is_empty());
    }

    #[test]
    fn missing_in_custodian() {
        let breaks = reconcile(&[("1".into(), 100.0)], &[]);
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].kind, BreakKind::MissingInCustodian);
        assert_eq!(breaks[0].diff, 100.0);
    }

    #[test]
    fn missing_in_oms() {
        let breaks = reconcile(&[], &[h(Some("1"), "SPY", 100.0)]);
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].kind, BreakKind::MissingInOms);
        assert_eq!(breaks[0].custodian_qty, 100.0);
    }

    #[test]
    fn qty_mismatch() {
        let breaks = reconcile(&[("1".into(), 100.0)], &[h(Some("1"), "SPY", 90.0)]);
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].kind, BreakKind::QtyMismatch);
        assert_eq!(breaks[0].diff, 10.0);
    }

    #[test]
    fn unresolved_custodian_security() {
        let breaks = reconcile(&[], &[h(None, "XYZ", 5.0)]);
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].kind, BreakKind::UnresolvedCustodianSecurity);
        assert_eq!(breaks[0].symbol.as_deref(), Some("XYZ"));
    }
}
