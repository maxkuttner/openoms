//! OMS-side instrument resolution: the `symbology` (OpenFIGI) engine + the DB.
//!
//! `resolve` is xref-first (a local `oms.instrument_xref` hit avoids OpenFIGI), then
//! falls back to the engine; on a hit it matches the FIGI identity to a master
//! `public.instrument`, stamps `figi`/`cusip`, and upserts the xref. Additive: it does
//! not touch the legacy `broker_instrument`/`provider_instrument` bridges or routing.

use serde::Serialize;
use sqlx::{PgPool, Row};
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

/// Resolve one query to a master instrument + FIGI, persisting the result.
pub async fn resolve(
    pool: &PgPool,
    engine: &SymbologyEngine,
    query: &InstrumentQuery,
    source_type: &str,
    source_code: &str,
) -> Result<ResolveOutcome, ResolveError> {
    let ext_symbol = query.ticker.as_deref();
    let ext_exchange = query.exch_code.as_deref().or(query.mic.as_deref());

    // 1) xref-first: a prior resolution for this (source, symbol, exchange).
    if let Some(row) = sqlx::query(
        "SELECT instrument_id, figi FROM instrument_xref \
         WHERE source_code = $1 \
           AND external_symbol IS NOT DISTINCT FROM $2 \
           AND external_exchange IS NOT DISTINCT FROM $3 \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(source_code)
    .bind(ext_symbol)
    .bind(ext_exchange)
    .fetch_optional(pool)
    .await?
    {
        if let Some(figi) = row.get::<Option<String>, _>("figi") {
            return Ok(ResolveOutcome::Resolved {
                instrument_id: row.get("instrument_id"),
                figi,
                identity: None, // cache hit — minimal
            });
        }
    }

    // 2) engine (OpenFIGI, cached in-process).
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
        // stamp the FIGI/CUSIP anchor on the master (narrow grant; only if empty).
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

    upsert_xref(pool, source_type, source_code, ext_symbol, ext_exchange, &identity, instrument_id).await?;

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

async fn upsert_xref(
    pool: &PgPool,
    source_type: &str,
    source_code: &str,
    symbol: Option<&str>,
    exchange: Option<&str>,
    identity: &InstrumentIdentity,
    instrument_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO instrument_xref \
           (instrument_id, source_type, source_code, external_symbol, external_exchange, figi, method, confidence) \
         VALUES ($1, $2, $3, $4, $5, $6, 'openfigi', 'resolved') \
         ON CONFLICT (source_type, source_code, \
                      COALESCE(external_native_id, ''), COALESCE(external_symbol, ''), COALESCE(external_exchange, '')) \
         DO UPDATE SET instrument_id = EXCLUDED.instrument_id, figi = EXCLUDED.figi, updated_at = now()",
    )
    .bind(instrument_id)
    .bind(source_type)
    .bind(source_code)
    .bind(symbol)
    .bind(exchange)
    .bind(&identity.figi)
    .execute(pool)
    .await?;
    Ok(())
}
