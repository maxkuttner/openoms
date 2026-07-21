//! Shared master-catalog upsert: turn provider-neutral [`InstrumentDef`]s into
//! `public.instrument` (+ `instrument_derivative`) rows, and optionally enrich them
//! with external identifiers (FIGI/CUSIP/ISIN via OpenFIGI).
//!
//! Broker-first seeding drives this from `setup::brokers`: an adapter's
//! `InstrumentProvider` yields the definitions, this module writes the master rows
//! and hands back the `(symbol, venue) -> id` map so the caller can attach the
//! `broker_instrument` mapping. It does not touch any symbology bridge table —
//! broker mapping lives in `broker_instrument`; feed mapping is derived at runtime
//! from each feed's own symbology, not stored.

use std::collections::{BTreeSet, HashMap, HashSet};

use dataprovider::{Enricher, InstrumentDef, OptionKind};
use sqlx::{PgPool, Postgres, Transaction};
use tracing::{info, warn};

/// Max rows per bulk statement. Arrays are passed as single binds, so this only
/// bounds statement/memory size, not the Postgres parameter limit.
const BATCH: usize = 4000;

#[derive(Default, Debug)]
pub struct CatalogSummary {
    pub upserted: u64,
    pub skipped_venue: u64,
    pub skipped_currency: u64,
    pub skipped_expired: u64,
    pub derivatives: u64,
    pub enriched: u64,
    /// The distinct venue/currency codes that failed the FK check. Distinct rather
    /// than per-row: a skip count alone says something is wrong but not what, and
    /// the answer is always a handful of codes even when thousands of rows drop.
    pub unknown_venues: BTreeSet<String>,
    pub unknown_currencies: BTreeSet<String>,
}

impl CatalogSummary {
    pub fn skipped_fk(&self) -> u64 {
        self.skipped_venue + self.skipped_currency
    }
}

/// Upsert a batch of instrument definitions into the master catalog. FK-filters on
/// the known venue/currency sets, drops already-expired options, dedups by
/// (symbol, venue), upserts instruments then derivatives in one transaction, then
/// runs the enricher pipeline. Returns the summary plus the `(symbol, venue) -> id`
/// map for every upserted row so the caller can write dependent mappings.
pub async fn upsert_catalog(
    pool: &PgPool,
    defs: &[InstrumentDef],
    enrichers: &[Box<dyn Enricher>],
) -> Result<(CatalogSummary, HashMap<(String, String), i64>), Box<dyn std::error::Error>> {
    let mut summary = CatalogSummary::default();
    let mut ids: HashMap<(String, String), i64> = HashMap::new();
    if defs.is_empty() {
        return Ok((summary, ids));
    }

    // FK filter in-process: load the valid venue/currency sets once instead of an
    // EXISTS round-trip per row.
    let venues: HashSet<String> = sqlx::query_scalar("SELECT code FROM venue")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    let currencies: HashSet<String> = sqlx::query_scalar("SELECT code FROM currency")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();

    // FK-filter, drop expired options, then dedup by the conflict key (symbol,
    // venue): a source can list the same instrument twice, which would make a bulk
    // `ON CONFLICT` touch a row twice ("cannot affect row a second time"). Last
    // write wins.
    let today = chrono::Utc::now().date_naive();
    let mut dedup: HashMap<(&str, &str), &InstrumentDef> = HashMap::with_capacity(defs.len());
    for d in defs {
        // Checked separately so the summary names which FK failed — "883 skipped"
        // cost a manual DB session to diagnose once already.
        if !venues.contains(&d.venue) {
            summary.skipped_venue += 1;
            summary.unknown_venues.insert(d.venue.clone());
            continue;
        }
        if !currencies.contains(&d.currency) {
            summary.skipped_currency += 1;
            summary.unknown_currencies.insert(d.currency.clone());
            continue;
        }
        if let Some(exp) = d.derivative.as_ref().and_then(|dv| dv.expiry_date) {
            if exp < today {
                summary.skipped_expired += 1;
                continue;
            }
        }
        dedup.insert((d.symbol.as_str(), d.venue.as_str()), d);
    }
    // Upsert SPOT before options so the underlying is visible to the derivative join
    // (same transaction sees its own writes).
    let mut valid: Vec<&InstrumentDef> = dedup.into_values().collect();
    valid.sort_by_key(|d| d.derivative.is_some());

    info!(
        "upserting {} instruments ({} skipped_fk, {} expired) …",
        valid.len(),
        summary.skipped_fk(),
        summary.skipped_expired
    );
    if !summary.unknown_venues.is_empty() {
        warn!(
            "{} row(s) skipped — no such venue: {:?} (seed the venue, or map the code in the adapter)",
            summary.skipped_venue, summary.unknown_venues
        );
    }
    if !summary.unknown_currencies.is_empty() {
        warn!(
            "{} row(s) skipped — no such currency: {:?} (add to db/scripts/seed_currencies.sql)",
            summary.skipped_currency, summary.unknown_currencies
        );
    }
    let mut tx = pool.begin().await?;
    for chunk in valid.chunks(BATCH) {
        let rows = bulk_upsert_instruments(&mut tx, chunk).await?;
        summary.upserted += rows.len() as u64;
        ids.extend(rows);
    }
    let derivs: Vec<&InstrumentDef> =
        valid.iter().copied().filter(|d| d.derivative.is_some()).collect();
    for chunk in derivs.chunks(BATCH) {
        summary.derivatives += bulk_upsert_derivatives(&mut tx, chunk, &ids).await? as u64;
    }
    tx.commit().await?;

    if !enrichers.is_empty() {
        summary.enriched = run_enrichers(pool, enrichers, defs).await?;
    }
    Ok((summary, ids))
}

/// Bulk-upsert a chunk of instruments via UNNEST, returning `(symbol, venue) -> id`
/// for every row (inserted or updated).
async fn bulk_upsert_instruments(
    tx: &mut Transaction<'_, Postgres>,
    chunk: &[&InstrumentDef],
) -> Result<Vec<((String, String), i64)>, Box<dyn std::error::Error>> {
    let symbol: Vec<String> = chunk.iter().map(|d| d.symbol.clone()).collect();
    let venue: Vec<String> = chunk.iter().map(|d| d.venue.clone()).collect();
    let currency: Vec<String> = chunk.iter().map(|d| d.currency.clone()).collect();
    let asset_class: Vec<String> = chunk.iter().map(|d| d.asset_class.clone()).collect();
    let instrument_class: Vec<String> = chunk.iter().map(|d| d.instrument_class.clone()).collect();
    let name: Vec<String> = chunk
        .iter()
        .map(|d| d.name.clone().unwrap_or_else(|| d.symbol.clone()))
        .collect();
    let price_precision: Vec<i32> = chunk.iter().map(|d| d.price_precision).collect();
    let price_increment: Vec<f64> = chunk.iter().map(|d| d.price_increment).collect();
    let size_increment: Vec<f64> = chunk.iter().map(|d| d.size_increment).collect();
    let lot_size: Vec<Option<f64>> = chunk.iter().map(|d| d.lot_size).collect();
    let contract_size: Vec<f64> = chunk.iter().map(|d| d.contract_size).collect();

    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "INSERT INTO instrument \
            (symbol, venue, currency, asset_class, instrument_class, name, \
             price_precision, size_precision, price_increment, size_increment, \
             lot_size, contract_size) \
         SELECT * FROM UNNEST( \
            $1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
            $7::int4[], array_fill(0::int4, ARRAY[array_length($1::text[], 1)]), \
            $8::float8[], $9::float8[], $10::float8[], $11::float8[]) \
         ON CONFLICT (symbol, venue) DO UPDATE SET \
            currency = EXCLUDED.currency, \
            asset_class = EXCLUDED.asset_class, \
            instrument_class = EXCLUDED.instrument_class, \
            name = EXCLUDED.name, \
            price_precision = EXCLUDED.price_precision, \
            price_increment = EXCLUDED.price_increment, \
            size_increment = EXCLUDED.size_increment, \
            lot_size = EXCLUDED.lot_size, \
            contract_size = EXCLUDED.contract_size, \
            updated_at = now() \
         RETURNING symbol, venue, id",
    )
    .bind(&symbol)
    .bind(&venue)
    .bind(&currency)
    .bind(&asset_class)
    .bind(&instrument_class)
    .bind(&name)
    .bind(&price_precision)
    .bind(&price_increment)
    .bind(&size_increment)
    .bind(&lot_size)
    .bind(&contract_size)
    .fetch_all(&mut **tx)
    .await?;

    Ok(rows.into_iter().map(|(s, v, id)| ((s, v), id)).collect())
}

/// Bulk-upsert derivative rows for a chunk of option legs. The underlying is
/// resolved by an inner join to the SPOT row, so an option whose underlying is
/// absent is skipped.
async fn bulk_upsert_derivatives(
    tx: &mut Transaction<'_, Postgres>,
    chunk: &[&InstrumentDef],
    ids: &HashMap<(String, String), i64>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut instrument_id: Vec<i64> = Vec::with_capacity(chunk.len());
    let mut underlying_symbol: Vec<String> = Vec::with_capacity(chunk.len());
    let mut option_kind: Vec<Option<String>> = Vec::with_capacity(chunk.len());
    let mut strike_price: Vec<Option<f64>> = Vec::with_capacity(chunk.len());
    let mut expiry_date: Vec<Option<chrono::NaiveDate>> = Vec::with_capacity(chunk.len());
    let mut activation_date: Vec<Option<chrono::NaiveDate>> = Vec::with_capacity(chunk.len());
    for d in chunk {
        let (Some(&id), Some(dv)) =
            (ids.get(&(d.symbol.clone(), d.venue.clone())), d.derivative.as_ref())
        else {
            continue;
        };
        instrument_id.push(id);
        underlying_symbol.push(dv.underlying_symbol.clone());
        option_kind.push(dv.option_kind.map(|k| match k {
            OptionKind::Call => "CALL".to_string(),
            OptionKind::Put => "PUT".to_string(),
        }));
        strike_price.push(dv.strike_price);
        expiry_date.push(dv.expiry_date);
        activation_date.push(dv.activation_date);
    }
    if instrument_id.is_empty() {
        return Ok(0);
    }

    let affected = sqlx::query(
        "INSERT INTO instrument_derivative \
            (instrument_id, underlying_id, underlying_symbol, \
             option_kind, strike_price, expiry_date, activation_date) \
         SELECT t.iid, u.id, t.us, t.ok, t.strike, t.exp, t.act \
         FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::float8[], \
                     $5::date[], $6::date[]) AS t(iid, us, ok, strike, exp, act) \
         JOIN instrument u ON u.symbol = t.us AND u.instrument_class = 'SPOT' \
         ON CONFLICT (instrument_id) DO UPDATE SET \
            underlying_id = EXCLUDED.underlying_id, \
            underlying_symbol = EXCLUDED.underlying_symbol, \
            option_kind = EXCLUDED.option_kind, \
            strike_price = EXCLUDED.strike_price, \
            expiry_date = EXCLUDED.expiry_date, \
            activation_date = EXCLUDED.activation_date",
    )
    .bind(&instrument_id)
    .bind(&underlying_symbol)
    .bind(&option_kind)
    .bind(&strike_price)
    .bind(&expiry_date)
    .bind(&activation_date)
    .execute(&mut **tx)
    .await?
    .rows_affected();

    Ok(affected as usize)
}

/// Run each enricher over the FIGI-less SPOT rows and persist the identifiers it
/// resolves onto the master. Returns the number of instruments stamped.
async fn run_enrichers(
    pool: &PgPool,
    enrichers: &[Box<dyn Enricher>],
    defs: &[InstrumentDef],
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut working: Vec<InstrumentDef> = Vec::new();
    for d in defs.iter().filter(|d| d.instrument_class == "SPOT") {
        let figi: Option<Option<String>> =
            sqlx::query_scalar("SELECT figi FROM instrument WHERE symbol = $1 AND venue = $2")
                .bind(&d.symbol)
                .bind(&d.venue)
                .fetch_optional(pool)
                .await?;
        if matches!(figi, Some(None)) {
            working.push(d.clone());
        }
    }
    if working.is_empty() {
        return Ok(0);
    }
    info!(
        "enriching {} SPOT symbol(s) via {} enricher(s)",
        working.len(),
        enrichers.len()
    );

    let mut stamped = 0u64;
    for enricher in enrichers {
        let report = enricher.enrich(&mut working).await?;
        for (symbol, venue) in &report.resolved {
            if let Some(d) = working.iter().find(|d| &d.symbol == symbol && &d.venue == venue) {
                persist_identifiers(pool, d).await?;
                stamped += 1;
            }
        }
    }
    Ok(stamped)
}

/// Stamp an enriched def's identifiers onto the master (only where still empty).
async fn persist_identifiers(pool: &PgPool, d: &InstrumentDef) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "UPDATE instrument SET \
            figi  = COALESCE(figi,  $3), \
            cusip = COALESCE(cusip, $4), \
            isin  = COALESCE(isin,  $5) \
         WHERE symbol = $1 AND venue = $2",
    )
    .bind(&d.symbol)
    .bind(&d.venue)
    .bind(d.identifiers.figi.as_deref())
    .bind(d.identifiers.cusip.as_deref())
    .bind(d.identifiers.isin.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}
