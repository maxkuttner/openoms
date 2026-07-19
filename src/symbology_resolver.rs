//! OMS-side symbology resolution: identify an external instrument via the
//! `symbology` (OpenFIGI) engine and stamp the FIGI/CUSIP anchor onto the master
//! `public.instrument`.
//!
//! This is enrichment, not mapping. The two mapping tables are owned elsewhere —
//! `broker_instrument` by broker sync, `feed_instrument` by feed mapping. The
//! resolver only answers "which master instrument is this, and what is its FIGI",
//! and records the FIGI anchor so later lookups are cheap. Off the order/quote hot
//! path.

use serde::Serialize;
use sqlx::PgPool;
use symbology::{InstrumentIdentity, InstrumentQuery, Resolution, SymbologyError};

use crate::app_state::SymbologyEngine;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ResolvedIdentity {
    pub figi: String,
    pub ticker: Option<String>,
    pub name: Option<String>,
    pub exch_code: Option<String>,
    pub security_type: Option<String>,
    pub market_sector: Option<String>,
    pub composite_figi: Option<String>,
    pub share_class_figi: Option<String>,
}

impl From<&InstrumentIdentity> for ResolvedIdentity {
    fn from(i: &InstrumentIdentity) -> Self {
        Self {
            figi: i.figi.clone(),
            ticker: i.ticker.clone(),
            name: i.name.clone(),
            exch_code: i.exch_code.clone(),
            security_type: i.security_type.clone(),
            market_sector: i.market_sector.clone(),
            composite_figi: i.composite_figi.clone(),
            share_class_figi: i.share_class_figi.clone(),
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ResolveOutcome {
    /// Identified; `instrument_id` is `None` if we hold no matching master instrument.
    Resolved {
        instrument_id: Option<i64>,
        figi: String,
        identity: Option<ResolvedIdentity>,
    },
    Ambiguous {
        candidates: Vec<ResolvedIdentity>,
    },
    Unresolved,
}

#[derive(Debug)]
pub enum ResolveError {
    Db(sqlx::Error),
    Engine(SymbologyError),
}

impl From<sqlx::Error> for ResolveError {
    fn from(e: sqlx::Error) -> Self {
        ResolveError::Db(e)
    }
}
impl From<SymbologyError> for ResolveError {
    fn from(e: SymbologyError) -> Self {
        ResolveError::Engine(e)
    }
}

/// Identify one query via OpenFIGI, match it to a master instrument, and stamp the
/// FIGI/CUSIP anchor onto that master (only where still empty).
pub async fn resolve(
    pool: &PgPool,
    engine: &SymbologyEngine,
    query: &InstrumentQuery,
) -> Result<ResolveOutcome, ResolveError> {
    let identity = match engine.identify(query).await? {
        Resolution::Resolved(i) => i,
        Resolution::Ambiguous(cands) => {
            return Ok(ResolveOutcome::Ambiguous {
                candidates: cands.iter().map(ResolvedIdentity::from).collect(),
            })
        }
        Resolution::NotFound => return Ok(ResolveOutcome::Unresolved),
    };

    let instrument_id = find_master(pool, &identity, query).await?;

    if let Some(id) = instrument_id {
        // Stamp the FIGI/CUSIP anchor on the master (narrow grant; only if empty).
        // NB: only figi/cusip are granted to the app role — do NOT touch updated_at.
        sqlx::query(
            "UPDATE instrument SET figi = COALESCE(figi, $2), cusip = COALESCE(cusip, $3) \
             WHERE id = $1",
        )
        .bind(id)
        .bind(&identity.figi)
        .bind(query.cusip.as_deref())
        .execute(pool)
        .await?;
    }

    Ok(ResolveOutcome::Resolved {
        instrument_id,
        figi: identity.figi.clone(),
        identity: Some(ResolvedIdentity::from(&identity)),
    })
}

/// Match a FIGI identity to a master instrument: by figi, then (symbol, venue), then unique symbol.
async fn find_master(
    pool: &PgPool,
    identity: &InstrumentIdentity,
    query: &InstrumentQuery,
) -> Result<Option<i64>, sqlx::Error> {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM instrument WHERE figi = $1 LIMIT 1")
        .bind(&identity.figi)
        .fetch_optional(pool)
        .await?
    {
        return Ok(Some(id));
    }

    let ticker = identity.ticker.as_deref().or(query.ticker.as_deref());
    if let (Some(ticker), Some(venue)) = (ticker, query.mic.as_deref()) {
        if let Some(id) =
            sqlx::query_scalar::<_, i64>("SELECT id FROM instrument WHERE symbol = $1 AND venue = $2 LIMIT 1")
                .bind(ticker)
                .bind(venue)
                .fetch_optional(pool)
                .await?
        {
            return Ok(Some(id));
        }
    }

    if let Some(ticker) = ticker {
        let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM instrument WHERE symbol = $1 AND status = 'ACTIVE'")
            .bind(ticker)
            .fetch_all(pool)
            .await?;
        if ids.len() == 1 {
            return Ok(Some(ids[0]));
        }
    }

    Ok(None)
}
