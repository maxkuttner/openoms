//! Strategies / algos that trade through the OMS (path ①): the registry.
//!
//! A strategy owns its portfolio exclusively, trades as its own service
//! principal, and logs on over FIX with its CompIDs and a password stored only
//! as a hash. `RegistrySnapshot` is the in-memory copy the FIX logon check and
//! the order path read without a query; every admin write reloads it.

pub mod admin;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use base64::{engine::general_purpose, Engine as _};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::domain::orders::risk::{RiskLimits, TradingState};

#[derive(Debug, Clone)]
pub struct Strategy {
    pub code: String,
    pub portfolio_id: Uuid,
    pub principal_id: Uuid,
    pub sender_comp_id: String,
    pub target_comp_id: String,
    pub fix_password_hash: Option<String>,
    pub allowed_addresses: Vec<String>,
    pub state: TradingState,
    pub rest_enabled: bool,
    pub limits: RiskLimits,
    pub price_band_pct: Option<f64>,
    /// broker_code → account_id: where its orders for that broker go.
    pub connections: BTreeMap<String, Uuid>,
}

const STRATEGY_SELECT: &str = "code, portfolio_id, principal_id, sender_comp_id, target_comp_id, \
    fix_password_hash, allowed_addresses, state, rest_enabled, \
    max_order_quantity::double precision    AS max_order_quantity, \
    max_order_notional::double precision    AS max_order_notional, \
    max_position_quantity::double precision AS max_position_quantity, \
    max_position_notional::double precision AS max_position_notional, \
    price_band_pct::double precision        AS price_band_pct";

pub(crate) fn parse_state(s: &str) -> Result<TradingState, sqlx::Error> {
    match s {
        "ACTIVE" => Ok(TradingState::Active),
        "REDUCING" => Ok(TradingState::Reducing),
        "HALTED" => Ok(TradingState::Halted),
        other => Err(sqlx::Error::Decode(format!("unknown strategy state {other}").into())),
    }
}

fn from_row(row: &sqlx::postgres::PgRow) -> Result<Strategy, sqlx::Error> {
    Ok(Strategy {
        code: row.try_get("code")?,
        portfolio_id: row.try_get("portfolio_id")?,
        principal_id: row.try_get("principal_id")?,
        sender_comp_id: row.try_get("sender_comp_id")?,
        target_comp_id: row.try_get("target_comp_id")?,
        fix_password_hash: row.try_get("fix_password_hash")?,
        allowed_addresses: row.try_get("allowed_addresses")?,
        state: parse_state(&row.try_get::<String, _>("state")?)?,
        rest_enabled: row.try_get("rest_enabled")?,
        limits: RiskLimits {
            max_order_quantity: row.try_get("max_order_quantity")?,
            max_order_notional: row.try_get("max_order_notional")?,
            max_position_quantity: row.try_get("max_position_quantity")?,
            max_position_notional: row.try_get("max_position_notional")?,
        },
        price_band_pct: row.try_get("price_band_pct")?,
        connections: BTreeMap::new(),
    })
}

async fn connections(conn: &mut PgConnection, code: &str) -> Result<BTreeMap<String, Uuid>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT broker_code, account_id FROM strategy_connection WHERE strategy_code = $1",
    )
    .bind(code)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn load_all(pool: &PgPool) -> Result<Vec<Strategy>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let rows = sqlx::query(&format!("SELECT {STRATEGY_SELECT} FROM strategy ORDER BY code"))
        .fetch_all(&mut *conn)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut s = from_row(row)?;
        s.connections = connections(&mut conn, &s.code).await?;
        out.push(s);
    }
    Ok(out)
}

/// The strategy that owns `portfolio_id`, if any, locking its row so
/// concurrent submits on the strategy serialize like a `risk_limits` row does.
pub async fn by_portfolio(conn: &mut PgConnection, portfolio_id: Uuid) -> Result<Option<Strategy>, sqlx::Error> {
    let row = sqlx::query(&format!("SELECT {STRATEGY_SELECT} FROM strategy WHERE portfolio_id = $1 FOR UPDATE"))
        .bind(portfolio_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let mut s = from_row(&row)?;
    s.connections = connections(conn, &s.code).await?;
    Ok(Some(s))
}

/// bcrypt, cost 12, as API keys are.
pub fn hash_fix_password(plain: &str) -> String {
    bcrypt::hash(plain, 12).expect("bcrypt hashing does not fail for in-range costs")
}

pub fn verify_fix_password(plain: &str, hash: &str) -> bool {
    bcrypt::verify(plain, hash).unwrap_or(false)
}

/// 24 URL-safe characters from 18 random bytes.
pub fn new_fix_password() -> String {
    let mut raw = [0u8; 18];
    raw[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    raw[16..].copy_from_slice(&Uuid::new_v4().as_bytes()[..2]);
    general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// The registry as last loaded, read without a query.
#[derive(Clone, Default)]
pub struct RegistrySnapshot(Arc<ArcSwap<Vec<Strategy>>>);

impl RegistrySnapshot {
    pub async fn reload(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        self.replace(load_all(pool).await?);
        Ok(())
    }

    pub(crate) fn replace(&self, strategies: Vec<Strategy>) {
        self.0.store(Arc::new(strategies));
    }

    pub fn all(&self) -> Arc<Vec<Strategy>> {
        self.0.load_full()
    }

    pub fn by_comp_ids(&self, sender: &str, target: &str) -> Option<Strategy> {
        self.0.load().iter().find(|s| s.sender_comp_id == sender && s.target_comp_id == target).cloned()
    }

    pub fn by_code(&self, code: &str) -> Option<Strategy> {
        self.0.load().iter().find(|s| s.code == code).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fix_password_verifies_against_its_hash_only() {
        let h = hash_fix_password("s3cret");
        assert!(verify_fix_password("s3cret", &h));
        assert!(!verify_fix_password("s3cre", &h));
        assert!(!h.contains("s3cret"));
    }

    #[test]
    fn snapshot_finds_a_strategy_by_its_comp_ids() {
        let snapshot = RegistrySnapshot::default();
        snapshot.replace(vec![sample("xarb", "XARB1", "OMS")]);
        assert_eq!(snapshot.by_comp_ids("XARB1", "OMS").map(|s| s.code), Some("xarb".to_string()));
        assert!(snapshot.by_comp_ids("XARB1", "X").is_none());
    }

    pub(crate) fn sample(code: &str, sender: &str, target: &str) -> Strategy {
        Strategy {
            code: code.into(),
            portfolio_id: uuid::Uuid::nil(),
            principal_id: uuid::Uuid::nil(),
            sender_comp_id: sender.into(),
            target_comp_id: target.into(),
            fix_password_hash: None,
            allowed_addresses: Vec::new(),
            state: crate::domain::orders::risk::TradingState::Active,
            rest_enabled: true,
            limits: crate::domain::orders::risk::RiskLimits {
                max_order_quantity: None,
                max_order_notional: None,
                max_position_quantity: None,
                max_position_notional: None,
            },
            price_band_pct: None,
            connections: Default::default(),
        }
    }
}
