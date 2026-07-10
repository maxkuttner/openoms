//! `oms setup universe` — seed the master instrument universe.
//!
//! The seeding framework: it is provider- and enricher-agnostic. It drives a
//! [`UniverseSource`] (Databento) and a `Vec<Box<dyn Enricher>>` (OpenFIGI today):
//!   1. Load enabled universes from `public.instrument_universe` (+ child symbols).
//!   2. Ask the provider for a free cost estimate per universe.
//!   3. Print a cost table + interactive y/N confirm.
//!   4. Fetch definitions, upsert `instrument` + `instrument_derivative` +
//!      `oms.instrument_xref` (PROVIDER rows).
//!   5. Run the enricher pipeline, persist `Identifiers` + one xref row per enricher.
//!   6. Write universe status back (`SEEDED` / `ERROR`).
//!
//! Adding a provider or a metadata source does not touch this file beyond
//! registering it in `run()`.

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Write as _};

use clap::Args as ClapArgs;
use dataprovider::{
    Category, CostEstimate, DataProvider, DatabentoClient, Enricher, InstrumentDef,
    OpenFigiEnricher, OptionKind, SType, UniverseSource, UniverseSpec,
};
use sqlx::{PgPool, Row};
use tracing::{info, warn};

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Seed exactly this universe code (skips the enabled filter).
    #[arg(long)]
    pub universe: Option<String>,
    /// Seed every universe with `enabled = true` in the catalog.
    #[arg(long)]
    pub all_enabled: bool,
    /// Estimate + print cost table only; no fetch, no writes.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the interactive y/N confirm.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Skip the enricher pipeline (FIGI via OpenFIGI).
    #[arg(long)]
    pub no_enrich: bool,
    /// Abort if the total estimated cost exceeds this value (USD).
    #[arg(long)]
    pub max_cost: Option<f64>,
}

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&database_url()?).await?;
    let universes = load_universes(&pool, args.universe.as_deref(), args.all_enabled).await?;
    if universes.is_empty() {
        info!("no universes to seed (use --universe CODE or --all-enabled).");
        return Ok(());
    }

    let db = DatabentoClient::from_env()?;
    db.set_catalog(universes.clone()).await;

    // Cost estimation (free metadata call).
    let mut estimates: Vec<CostEstimate> = Vec::with_capacity(universes.len());
    for u in &universes {
        estimates.push(db.estimate_cost(u).await?);
    }
    let total: f64 = estimates.iter().map(|c| c.usd).sum();
    print_cost_table(&universes, &estimates, total);

    if let Some(max) = args.max_cost {
        if total > max {
            return Err(format!("estimated cost ${total:.4} exceeds --max-cost ${max:.4}").into());
        }
    }
    if args.dry_run {
        info!("dry run: {} universe(s), estimated ${total:.4}. no fetch or write.", universes.len());
        return Ok(());
    }
    if !args.yes && !confirm_prompt(universes.len(), total)? {
        info!("aborted.");
        return Ok(());
    }

    // The enricher pipeline. Push a new `Box<dyn Enricher>` here to add a
    // metadata/identifier source — the rest of the framework is untouched.
    let enrichers: Vec<Box<dyn Enricher>> = if args.no_enrich {
        Vec::new()
    } else {
        vec![Box::new(OpenFigiEnricher::new(env::var("OPENFIGI_API_KEY").ok()))]
    };

    let mut failures: Vec<String> = Vec::new();
    for u in &universes {
        if let Err(e) = require_underlyings(u) {
            warn!("skipping {}: {e}", u.code);
            set_status(&pool, &u.code, "ERROR", None, Some(&e.to_string())).await?;
            failures.push(u.code.clone());
            continue;
        }
        info!("seeding universe {} …", u.code);
        set_status(&pool, &u.code, "SEEDING", None, None).await?;

        match seed_one(&pool, &db, u, &enrichers).await {
            Ok(summary) => {
                info!(
                    "{}: upserted={} skipped_fk={} skipped_expired={} derivatives={} provider_xref={} enriched={}",
                    u.code,
                    summary.upserted,
                    summary.skipped_fk,
                    summary.skipped_expired,
                    summary.derivatives,
                    summary.provider_xref,
                    summary.enriched
                );
                set_status(&pool, &u.code, "SEEDED", Some(summary.upserted as i32), None).await?;
            }
            Err(e) => {
                warn!("{} failed: {e}", u.code);
                set_status(&pool, &u.code, "ERROR", None, Some(&e.to_string())).await?;
                failures.push(u.code.clone());
            }
        }
    }

    if !failures.is_empty() {
        return Err(format!("seeding failed for: {}", failures.join(", ")).into());
    }
    Ok(())
}

// ------------------------------------------------------------------------
// Reusable core (shared by the CLI above and the admin API)
// ------------------------------------------------------------------------

/// Free cost estimate for a single universe. `Ok(None)` if the code is unknown.
pub async fn estimate(
    pool: &PgPool,
    code: &str,
) -> Result<Option<CostEstimate>, Box<dyn std::error::Error>> {
    let mut universes = load_universes(pool, Some(code), false).await?;
    let Some(spec) = universes.pop() else {
        return Ok(None);
    };
    require_underlyings(&spec)?;
    let db = DatabentoClient::from_env()?;
    db.set_catalog(vec![spec.clone()]).await;
    Ok(Some(db.estimate_cost(&spec).await?))
}

/// OPTION universes must name their underlyings — seeding the whole OPRA tape
/// (`ALL`) is ~1.5M contracts and not allowed. Equity/future universes may be
/// whole-dataset.
fn require_underlyings(spec: &UniverseSpec) -> Result<(), Box<dyn std::error::Error>> {
    if matches!(spec.category, Category::Option) && spec.symbols.is_empty() {
        return Err(format!(
            "OPTION universe {} has no underlyings — pick underlyings before seeding (ALL is not allowed for options)",
            spec.code
        )
        .into());
    }
    Ok(())
}

/// Seed a single universe end to end: (optionally) gate on cost → fetch → upsert
/// → enrich, writing the universe's seed-state (`SEEDING` → `SEEDED`/`ERROR`) as
/// it goes. Intended for the admin API's background task — everything, including
/// the cost estimate/gate, runs here so the HTTP request returns immediately.
/// Runs in a spawned task, so the returned error must be `Send` — use `String`
/// rather than a `Box<dyn Error>` that would poison the future's `Send` bound.
pub async fn seed(
    pool: &PgPool,
    code: &str,
    enrich: bool,
    max_cost: Option<f64>,
) -> Result<(), String> {
    let mut universes = load_universes(pool, Some(code), false)
        .await
        .map_err(|e| e.to_string())?;
    let Some(spec) = universes.pop() else {
        return Err(format!("unknown universe: {code}"));
    };
    require_underlyings(&spec).map_err(|e| e.to_string())?;

    let db = DatabentoClient::from_env().map_err(|e| e.to_string())?;
    db.set_catalog(vec![spec.clone()]).await;

    // Cost gate — only estimate when there is a budget to enforce. Skipping it
    // otherwise avoids a dependency on Databento's flaky metadata.get_cost and
    // keeps the definition-schema seed (≈$0) fast.
    if let Some(max) = max_cost {
        let est = db.estimate_cost(&spec).await.map_err(|e| e.to_string())?;
        if est.usd > max {
            let msg = format!("estimated ${:.4} exceeds max_cost ${max:.4}", est.usd);
            set_status(pool, &spec.code, "ERROR", None, Some(&msg))
                .await
                .map_err(|e| e.to_string())?;
            return Err(msg);
        }
    }

    let enrichers: Vec<Box<dyn Enricher>> = if enrich {
        vec![Box::new(OpenFigiEnricher::new(env::var("OPENFIGI_API_KEY").ok()))]
    } else {
        Vec::new()
    };

    info!(
        "seeding universe {} (enrich={}, {} underlying(s)) …",
        spec.code,
        enrich,
        spec.symbols.len()
    );
    set_status(pool, &spec.code, "SEEDING", None, None)
        .await
        .map_err(|e| e.to_string())?;
    match seed_one(pool, &db, &spec, &enrichers).await.map_err(|e| e.to_string()) {
        Ok(summary) => {
            info!(
                "{}: upserted={} skipped_fk={} skipped_expired={} derivatives={} provider_xref={} enriched={}",
                spec.code,
                summary.upserted,
                summary.skipped_fk,
                summary.skipped_expired,
                summary.derivatives,
                summary.provider_xref,
                summary.enriched
            );
            set_status(pool, &spec.code, "SEEDED", Some(summary.upserted as i32), None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(msg) => {
            warn!("{} failed: {msg}", spec.code);
            set_status(pool, &spec.code, "ERROR", None, Some(&msg))
                .await
                .map_err(|e| e.to_string())?;
            Err(msg)
        }
    }
}

// ------------------------------------------------------------------------
// Per-universe work
// ------------------------------------------------------------------------

#[derive(Default)]
struct SeedSummary {
    upserted: u64,
    skipped_fk: u64,
    skipped_expired: u64,
    derivatives: u64,
    provider_xref: u64,
    enriched: u64,
}

async fn seed_one(
    pool: &PgPool,
    db: &DatabentoClient,
    spec: &UniverseSpec,
    enrichers: &[Box<dyn Enricher>],
) -> Result<SeedSummary, Box<dyn std::error::Error>> {
    let t_fetch = std::time::Instant::now();
    let defs = db.fetch_definitions(spec).await?;
    info!(
        "{}: fetched {} definitions in {:.1}s",
        spec.code,
        defs.len(),
        t_fetch.elapsed().as_secs_f64()
    );
    if defs.is_empty() {
        info!("{}: no mappable instruments.", spec.code);
        return Ok(SeedSummary::default());
    }

    let mut summary = SeedSummary::default();

    // FK filter in-process: load the valid venue/currency sets once instead of
    // an EXISTS round-trip per row.
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

    // FK-filter, then dedup by the instrument conflict key (symbol, venue): a
    // multi-day definition range can return the same instrument more than once,
    // which would make a bulk `ON CONFLICT` statement touch a row twice
    // ("cannot affect row a second time"). Last write wins. This key also covers
    // the xref and derivative statements, whose conflict keys derive from it.
    let today = chrono::Utc::now().date_naive();
    let mut dedup: HashMap<(&str, &str), &InstrumentDef> = HashMap::with_capacity(defs.len());
    for d in &defs {
        if !(venues.contains(&d.venue) && currencies.contains(&d.currency)) {
            summary.skipped_fk += 1;
            continue;
        }
        // Skip already-expired options — dead contracts, not worth seeding. Only
        // drops when an expiry is present and in the past; missing expiry is kept.
        if let Some(exp) = d.derivative.as_ref().and_then(|dv| dv.expiry_date) {
            if exp < today {
                summary.skipped_expired += 1;
                continue;
            }
        }
        dedup.insert((d.symbol.as_str(), d.venue.as_str()), d);
    }
    // Upsert equities before options so the underlying SPOT row is visible to the
    // derivative join (same transaction sees its own writes).
    let mut valid: Vec<&InstrumentDef> = dedup.into_values().collect();
    valid.sort_by_key(|d| d.derivative.is_some());

    info!(
        "{}: upserting {} instruments ({} skipped_fk, {} expired) …",
        spec.code,
        valid.len(),
        summary.skipped_fk,
        summary.skipped_expired
    );
    let t_upsert = std::time::Instant::now();
    let mut tx = pool.begin().await?;

    // 1. Bulk-upsert instruments, chunked; collect the (symbol, venue) -> id map.
    let mut ids: HashMap<(String, String), i64> = HashMap::with_capacity(valid.len());
    for chunk in valid.chunks(BATCH) {
        let rows = bulk_upsert_instruments(&mut tx, chunk).await?;
        summary.upserted += rows.len() as u64;
        ids.extend(rows);
    }

    // 2. Bulk-upsert PROVIDER xref rows.
    for chunk in valid.chunks(BATCH) {
        summary.provider_xref +=
            bulk_upsert_xref(&mut tx, db.code(), chunk, &ids).await? as u64;
    }

    // 3. Bulk-upsert derivative rows for the option legs.
    let derivs: Vec<&InstrumentDef> = valid.iter().copied().filter(|d| d.derivative.is_some()).collect();
    for chunk in derivs.chunks(BATCH) {
        summary.derivatives +=
            bulk_upsert_derivatives(&mut tx, chunk, &ids).await? as u64;
    }

    tx.commit().await?;
    info!(
        "{}: upserted {} instruments ({} derivatives, {} xref) in {:.1}s",
        spec.code,
        summary.upserted,
        summary.derivatives,
        summary.provider_xref,
        t_upsert.elapsed().as_secs_f64()
    );

    // Enricher pipeline (fills Identifiers, persists figi/cusip/isin + xref).
    if !enrichers.is_empty() {
        let t_enrich = std::time::Instant::now();
        summary.enriched = run_enrichers(pool, enrichers, &defs).await?;
        info!(
            "{}: enriched {} instruments in {:.1}s",
            spec.code,
            summary.enriched,
            t_enrich.elapsed().as_secs_f64()
        );
    }

    Ok(summary)
}

/// Max rows per bulk statement. Arrays are passed as single binds, so this only
/// bounds statement/memory size, not the Postgres parameter limit.
const BATCH: usize = 4000;

/// Bulk-upsert a chunk of instruments via UNNEST, returning `(symbol, venue) -> id`
/// for every row (inserted or updated).
async fn bulk_upsert_instruments(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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

/// Bulk-upsert PROVIDER xref rows for a chunk. Returns the number written.
async fn bulk_upsert_xref(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    source_code: &str,
    chunk: &[&InstrumentDef],
    ids: &HashMap<(String, String), i64>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut instrument_id: Vec<i64> = Vec::with_capacity(chunk.len());
    let mut external_symbol: Vec<String> = Vec::with_capacity(chunk.len());
    let mut external_exchange: Vec<Option<String>> = Vec::with_capacity(chunk.len());
    let mut external_native_id: Vec<Option<String>> = Vec::with_capacity(chunk.len());
    for d in chunk {
        let Some(&id) = ids.get(&(d.symbol.clone(), d.venue.clone())) else {
            continue;
        };
        instrument_id.push(id);
        external_symbol.push(d.symbol.clone());
        external_exchange.push(d.provider_exchange.clone());
        external_native_id.push(d.native_id.clone());
    }
    if instrument_id.is_empty() {
        return Ok(0);
    }

    sqlx::query(
        "INSERT INTO oms.instrument_xref \
            (instrument_id, source_type, source_code, external_symbol, \
             external_exchange, external_native_id, method, confidence) \
         SELECT t.iid, 'PROVIDER', $1, t.sym, t.exch, t.nid, 'seed_instruments', 'resolved' \
         FROM UNNEST($2::bigint[], $3::text[], $4::text[], $5::text[]) \
              AS t(iid, sym, exch, nid) \
         ON CONFLICT (source_type, source_code, \
                      COALESCE(external_native_id, ''), \
                      COALESCE(external_symbol, ''), \
                      COALESCE(external_exchange, '')) \
         DO UPDATE SET instrument_id = EXCLUDED.instrument_id, updated_at = now()",
    )
    .bind(source_code)
    .bind(&instrument_id)
    .bind(&external_symbol)
    .bind(&external_exchange)
    .bind(&external_native_id)
    .execute(&mut **tx)
    .await?;

    Ok(instrument_id.len())
}

/// Bulk-upsert derivative rows for a chunk of option legs. The underlying is
/// resolved by an inner join to the SPOT row, so an option whose underlying is
/// absent is skipped (matches the prior per-row behavior).
async fn bulk_upsert_derivatives(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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

/// Run each enricher over the FIGI-less SPOT rows and persist what they resolve.
/// Returns the number of `(instrument, enricher)` stamps written.
async fn run_enrichers(
    pool: &PgPool,
    enrichers: &[Box<dyn Enricher>],
    defs: &[InstrumentDef],
) -> Result<u64, Box<dyn std::error::Error>> {
    // Working set: SPOT rows still missing a FIGI in the master (idempotent).
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
        "enriching {} SPOT symbol(s) via {} enricher(s) — this is the slow phase (external lookups + per-row writes)",
        working.len(),
        enrichers.len()
    );

    let mut stamped = 0u64;
    for enricher in enrichers {
        let report = enricher.enrich(&mut working).await?;
        let total = report.resolved.len();
        for (i, (symbol, venue)) in report.resolved.iter().enumerate() {
            if let Some(d) = working.iter().find(|d| &d.symbol == symbol && &d.venue == venue) {
                persist_identifiers(pool, d, enricher.code()).await?;
                stamped += 1;
            }
            if (i + 1) % 500 == 0 {
                info!("  {} enrich: persisted {}/{}", enricher.code(), i + 1, total);
            }
        }
    }
    Ok(stamped)
}

/// Persist an enriched def's identifiers onto the master and stamp one xref row
/// attributed to the enricher (`source_code`).
async fn persist_identifiers(
    pool: &PgPool,
    d: &InstrumentDef,
    source_code: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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

    sqlx::query(
        "INSERT INTO oms.instrument_xref \
            (instrument_id, source_type, source_code, external_symbol, \
             external_exchange, figi, method, confidence) \
         SELECT i.id, $3, $3, i.symbol, i.venue, $4, 'enrich', 'resolved' \
         FROM instrument i WHERE i.symbol = $1 AND i.venue = $2 \
         ON CONFLICT (source_type, source_code, \
                      COALESCE(external_native_id, ''), \
                      COALESCE(external_symbol, ''), \
                      COALESCE(external_exchange, '')) \
         DO UPDATE SET instrument_id = EXCLUDED.instrument_id, \
                       figi = EXCLUDED.figi, updated_at = now()",
    )
    .bind(&d.symbol)
    .bind(&d.venue)
    .bind(source_code)
    .bind(d.identifiers.figi.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------------
// Catalog IO
// ------------------------------------------------------------------------

async fn load_universes(
    pool: &PgPool,
    code: Option<&str>,
    all_enabled: bool,
) -> Result<Vec<UniverseSpec>, Box<dyn std::error::Error>> {
    let rows = if let Some(code) = code {
        sqlx::query(
            "SELECT code, description, category, dataset, option_dataset, stype_in, \
                    include_options \
             FROM instrument_universe WHERE code = $1",
        )
        .bind(code)
        .fetch_all(pool)
        .await?
    } else if all_enabled {
        sqlx::query(
            "SELECT code, description, category, dataset, option_dataset, stype_in, \
                    include_options \
             FROM instrument_universe WHERE enabled = true \
             ORDER BY category, code",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT code, description, category, dataset, option_dataset, stype_in, \
                    include_options \
             FROM instrument_universe ORDER BY category, code",
        )
        .fetch_all(pool)
        .await?
    };

    let mut out: Vec<UniverseSpec> = Vec::with_capacity(rows.len());
    for r in rows {
        let code: String = r.get("code");
        let symbols: Vec<String> = sqlx::query_scalar(
            "SELECT symbol FROM instrument_universe_symbol \
             WHERE universe_code = $1 ORDER BY symbol",
        )
        .bind(&code)
        .fetch_all(pool)
        .await?;
        let stype_in_s: String = r.get("stype_in");
        let cat_s: String = r.get("category");
        out.push(UniverseSpec {
            code,
            description: r.get("description"),
            category: parse_category(&cat_s)?,
            dataset: r.get("dataset"),
            option_dataset: r.get("option_dataset"),
            symbols,
            stype_in: match stype_in_s.as_str() {
                "parent" => SType::Parent,
                _ => SType::RawSymbol,
            },
            include_options: r.get("include_options"),
        });
    }
    Ok(out)
}

fn parse_category(s: &str) -> Result<Category, Box<dyn std::error::Error>> {
    Ok(match s {
        "EQUITY" => Category::Equity,
        "OPTION" => Category::Option,
        "FUTURE" => Category::Future,
        other => return Err(format!("unknown universe category: {other}").into()),
    })
}

async fn set_status(
    pool: &PgPool,
    code: &str,
    status: &str,
    count: Option<i32>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE instrument_universe SET \
            status = $2, \
            last_seeded_at = CASE WHEN $2 = 'SEEDED' THEN now() ELSE last_seeded_at END, \
            instrument_count = COALESCE($3, instrument_count), \
            last_error = $4, \
            updated_at = now() \
         WHERE code = $1",
    )
    .bind(code)
    .bind(status)
    .bind(count)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------------
// UI helpers
// ------------------------------------------------------------------------

fn print_cost_table(universes: &[UniverseSpec], estimates: &[CostEstimate], total: f64) {
    println!("\nEstimated Databento cost (definition schema, free to estimate):\n");
    for (u, c) in universes.iter().zip(estimates) {
        let nsym = if u.symbols.is_empty() {
            "ALL".to_string()
        } else {
            u.symbols.len().to_string()
        };
        println!(
            "  {:<16} {:<7} sym={:<4} ${:>10.4}",
            u.code,
            format!("{:?}", u.category).to_uppercase(),
            nsym,
            c.usd,
        );
    }
    println!("  {:<30} ${:>10.4}\n", "TOTAL", total);
}

fn confirm_prompt(n: usize, total: f64) -> io::Result<bool> {
    print!("Seed {n} universe(s) for ~${total:.4}? [y/N]: ");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().eq_ignore_ascii_case("y"))
}

fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    let var = |k: &str| env::var(k).map_err(|_| format!("{k} must be set"));
    Ok(format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=disable",
        var("DB_USER")?,
        var("DB_PASSWORD")?,
        var("DB_HOST")?,
        var("DB_PORT")?,
        var("DB_NAME")?,
    ))
}
