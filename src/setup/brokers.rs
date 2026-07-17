//! `oms setup sync-brokers` — sync a broker's instrument catalog into
//! `oms.instrument_xref` (source_type='BROKER'), the map the order path resolves
//! `instrument_id -> broker handle` through at routing time.
//!
//! Ports the former `scripts/broker_sync.py` into Rust: it reuses the Alpaca
//! adapter's HTTP/auth (`AlpacaAdapter::list_equity_instruments` /
//! `list_option_contracts`) and the universe seeder's UNNEST bulk-upsert idiom.
//! Runs as the ordinary `DB_USER` (oms_user owns the `oms` schema), so unlike the
//! Python script it needs no superuser.

use std::collections::HashMap;
use std::env;

use clap::{Args as ClapArgs, ValueEnum};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tracing::{info, warn};

use crate::adapters::alpaca::AlpacaAdapter;
use crate::adapters::BrokerInstrument;

const BROKER_CODE: &str = "ALPACA";
const BATCH: usize = 4000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum AssetClass {
    Equity,
    Option,
    All,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Which broker catalog to sync.
    #[arg(long, value_enum, default_value_t = AssetClass::All)]
    pub asset_class: AssetClass,
    /// Comma-separated option underlyings (only used for the option leg).
    #[arg(long, default_value = "SPY,QQQ")]
    pub underlyings: String,
    /// Fetch + match + print counts, but write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&super::database_url()?).await?;
    let adapter = build_alpaca()?;
    let underlyings: Vec<String> = args
        .underlyings
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();

    if matches!(args.asset_class, AssetClass::Equity | AssetClass::All) {
        sync_equities(&pool, &adapter, args.dry_run).await?;
    }
    if matches!(args.asset_class, AssetClass::Option | AssetClass::All) {
        sync_options(&pool, &adapter, &underlyings, args.dry_run).await?;
    }
    Ok(())
}

/// Build an `AlpacaAdapter` standalone from env, matching the server's wiring
/// (`ALPACA_ENV` + `ALPACA_{ENV}_API_KEY/SECRET`).
fn build_alpaca() -> Result<AlpacaAdapter, Box<dyn std::error::Error>> {
    let env_name = env::var("ALPACA_ENV").unwrap_or_else(|_| "PAPER".into()).to_uppercase();
    let key = env::var(format!("ALPACA_{env_name}_API_KEY"))
        .map_err(|_| format!("ALPACA_{env_name}_API_KEY must be set"))?;
    let secret = env::var(format!("ALPACA_{env_name}_API_SECRET"))
        .map_err(|_| format!("ALPACA_{env_name}_API_SECRET must be set"))?;
    Ok(AlpacaAdapter::new(key, secret, &env_name))
}

/// A catalog row matched to a master instrument, ready to upsert.
struct BrokerXrefRow {
    instrument_id: i64,
    symbol: String,
    exchange: Option<String>,
    native_id: Option<String>,
    is_tradeable: bool,
    min_quantity: Option<f64>,
}

fn to_row(instrument_id: i64, bi: &BrokerInstrument) -> BrokerXrefRow {
    BrokerXrefRow {
        instrument_id,
        symbol: bi.symbol.clone(),
        exchange: bi.exchange.clone(),
        native_id: bi.native_id.clone(),
        is_tradeable: bi.is_tradeable,
        min_quantity: bi.min_quantity,
    }
}

/// Databento OPRA `raw_symbol` (21-char OCC OSI, root right-padded with spaces)
/// -> Alpaca's compact OSI. The only spaces are the root padding, so stripping
/// them yields Alpaca's `symbol` (`SPY   260713P00775000` -> `SPY260713P00775000`).
fn compact_osi(sym: &str) -> String {
    sym.replace(' ', "")
}

async fn sync_equities(
    pool: &PgPool,
    adapter: &AlpacaAdapter,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // symbol -> master id for the equity (SPOT) universe; Alpaca tickers match 1:1.
    let id_map: HashMap<String, i64> = sqlx::query(
        "SELECT symbol, id FROM instrument WHERE instrument_class = 'SPOT'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.get::<String, _>("symbol"), r.get::<i64, _>("id")))
    .collect();
    info!("loaded {} SPOT instrument(s) from master", id_map.len());

    let catalog = adapter.list_equity_instruments().await?;
    info!("fetched {} active {BROKER_CODE} equity asset(s)", catalog.len());

    let rows: Vec<BrokerXrefRow> = catalog
        .iter()
        .filter_map(|bi| id_map.get(&bi.symbol).map(|&id| to_row(id, bi)))
        .collect();
    info!("matched {} equity asset(s) to master ({} unmatched)", rows.len(), catalog.len() - rows.len());

    write_rows(pool, &rows, dry_run, "equity").await
}

async fn sync_options(
    pool: &PgPool,
    adapter: &AlpacaAdapter,
    underlyings: &[String],
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // compact OSI -> master id, scoped to the requested underlyings.
    let id_map: HashMap<String, i64> = sqlx::query(
        "SELECT i.id, i.symbol \
         FROM instrument i \
         JOIN instrument_derivative d ON d.instrument_id = i.id \
         WHERE i.instrument_class = 'OPTION' AND d.underlying_symbol = ANY($1)",
    )
    .bind(underlyings)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (compact_osi(&r.get::<String, _>("symbol")), r.get::<i64, _>("id")))
    .collect();
    info!("loaded {} OPTION instrument(s) for {underlyings:?}", id_map.len());

    let catalog = adapter.list_option_contracts(underlyings).await?;
    info!("fetched {} active {BROKER_CODE} option contract(s)", catalog.len());

    let rows: Vec<BrokerXrefRow> = catalog
        .iter()
        .filter_map(|bi| id_map.get(&bi.symbol).map(|&id| to_row(id, bi)))
        .collect();
    info!("matched {} contract(s) to master ({} unmatched)", rows.len(), catalog.len() - rows.len());

    write_rows(pool, &rows, dry_run, "option").await
}

async fn write_rows(
    pool: &PgPool,
    rows: &[BrokerXrefRow],
    dry_run: bool,
    label: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if dry_run {
        for r in rows.iter().take(20) {
            info!("  {} -> instrument_id={} tradeable={}", r.symbol, r.instrument_id, r.is_tradeable);
        }
        info!("dry run complete ({label}): {} row(s), no write", rows.len());
        return Ok(());
    }
    if rows.is_empty() {
        warn!("no {label} rows matched; nothing to upsert");
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    let mut n = 0usize;
    for chunk in rows.chunks(BATCH) {
        n += bulk_upsert_broker_xref(&mut tx, chunk).await?;
    }
    tx.commit().await?;
    info!("broker sync done: upserted {n} {BROKER_CODE} {label} xref row(s)");
    Ok(())
}

/// Bulk-upsert BROKER xref rows via UNNEST — mirrors `universe::bulk_upsert_xref`
/// but writes source_type='BROKER' plus the broker-routing columns
/// (`is_tradeable`, `min_quantity`). Same ON CONFLICT key.
async fn bulk_upsert_broker_xref(
    tx: &mut Transaction<'_, Postgres>,
    chunk: &[BrokerXrefRow],
) -> Result<usize, Box<dyn std::error::Error>> {
    let instrument_id: Vec<i64> = chunk.iter().map(|r| r.instrument_id).collect();
    let symbol: Vec<String> = chunk.iter().map(|r| r.symbol.clone()).collect();
    let exchange: Vec<Option<String>> = chunk.iter().map(|r| r.exchange.clone()).collect();
    let native_id: Vec<Option<String>> = chunk.iter().map(|r| r.native_id.clone()).collect();
    let is_tradeable: Vec<bool> = chunk.iter().map(|r| r.is_tradeable).collect();
    let min_quantity: Vec<Option<f64>> = chunk.iter().map(|r| r.min_quantity).collect();

    sqlx::query(
        "INSERT INTO oms.instrument_xref \
            (instrument_id, source_type, source_code, external_symbol, external_exchange, \
             external_native_id, is_tradeable, min_quantity, method, confidence) \
         SELECT t.iid, 'BROKER', $1, t.sym, t.exch, t.nid, t.trad, t.minq, 'broker_sync', 'resolved' \
         FROM UNNEST($2::bigint[], $3::text[], $4::text[], $5::text[], $6::bool[], $7::float8[]) \
              AS t(iid, sym, exch, nid, trad, minq) \
         ON CONFLICT (source_type, source_code, \
                      COALESCE(external_symbol, ''), \
                      COALESCE(external_exchange, '')) \
         DO UPDATE SET instrument_id      = EXCLUDED.instrument_id, \
                       external_native_id = EXCLUDED.external_native_id, \
                       is_tradeable       = EXCLUDED.is_tradeable, \
                       min_quantity       = EXCLUDED.min_quantity, \
                       updated_at         = now()",
    )
    .bind(BROKER_CODE)
    .bind(&instrument_id)
    .bind(&symbol)
    .bind(&exchange)
    .bind(&native_id)
    .bind(&is_tradeable)
    .bind(&min_quantity)
    .execute(&mut **tx)
    .await?;

    Ok(chunk.len())
}

#[cfg(test)]
mod tests {
    use super::compact_osi;

    #[test]
    fn compact_osi_strips_root_padding() {
        // Databento space-padded OSI -> Alpaca compact OSI.
        assert_eq!(compact_osi("SPY   260713P00775000"), "SPY260713P00775000");
        assert_eq!(compact_osi("QQQ   260713C00495000"), "QQQ260713C00495000");
        // 6-char root has no padding — unchanged.
        assert_eq!(compact_osi("SPXW  260713C05000000"), "SPXW260713C05000000");
        // Already compact is idempotent.
        assert_eq!(compact_osi("SPY260713P00775000"), "SPY260713P00775000");
    }
}
