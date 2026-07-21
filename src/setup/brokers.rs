//! `oms setup sync-broker` — the instrument seeding path.
//!
//! Broker-first: an adapter's [`InstrumentProvider`] enumerates the
//! broker's tradeable catalog; we create the master `public.instrument` (+
//! `instrument_derivative`) rows and the `public.broker_instrument` routing mapping
//! in one pass. The broker is the authoritative source of the instrument — there is
//! no separate dataset catalog and no priceable-but-not-tradeable path.
//!
//! Runs as the ordinary `DB_USER` (oms_user), which holds write on the master
//! catalog and both mapping tables (see `db/access/ods.sql`).

use std::env;

use clap::{Args as ClapArgs, ValueEnum};
use dataprovider::{Enricher, InstrumentDef, OpenFigiEnricher};
use sqlx::{PgPool, Postgres, Transaction};
use tracing::{info, warn};

use crate::adapters::alpaca::AlpacaAdapter;
use crate::adapters::binance::BinanceAdapter;
use crate::adapters::{BrokerInstrument, InstrumentProvider};
use crate::setup::catalog;

const BATCH: usize = 4000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Broker {
    Alpaca,
    Binance,
}

impl Broker {
    fn code(self) -> &'static str {
        match self {
            Broker::Alpaca => "ALPACA",
            Broker::Binance => "BINANCE",
        }
    }
}

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Which broker's catalog to sync (also the instrument source).
    #[arg(long, value_enum, default_value_t = Broker::Alpaca)]
    pub broker: Broker,
    /// Comma-separated option underlyings to seed the option chain for (Alpaca only).
    /// Empty seeds equities/spot only — the full option tape is not seeded wholesale.
    #[arg(long, default_value = "")]
    pub underlyings: String,
    /// Skip the enricher pipeline (FIGI via OpenFIGI).
    #[arg(long)]
    pub no_enrich: bool,
    /// Fetch + match + print counts, but write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Exit 0 even when instruments were skipped on a missing venue/currency.
    /// Without this, any FK skip fails the run: a partial catalog that reports
    /// success is how a seeding gap survives to become a missing price.
    #[arg(long)]
    pub allow_skips: bool,
}

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&super::database_url()?).await?;
    let underlyings: Vec<String> = args
        .underlyings
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();

    let provider: Box<dyn InstrumentProvider> = match args.broker {
        Broker::Alpaca => Box::new(build_alpaca()?),
        Broker::Binance => Box::new(build_binance()?),
    };

    info!("fetching {} catalog …", args.broker.code());
    let catalog = provider.list_instruments(&underlyings).await?;
    info!("fetched {} tradeable instrument(s) from {}", catalog.len(), args.broker.code());

    if args.dry_run {
        for bi in catalog.iter().take(20) {
            info!(
                "  {}@{} -> broker_symbol={} tradeable={}",
                bi.definition.symbol, bi.definition.venue, bi.broker_symbol, bi.is_tradeable
            );
        }
        info!("dry run complete: {} instrument(s), no write", catalog.len());
        return Ok(());
    }
    if catalog.is_empty() {
        warn!("empty catalog; nothing to seed");
        return Ok(());
    }

    // 1) Create/refresh the master instrument catalog from the broker's definitions.
    let defs: Vec<InstrumentDef> = catalog.iter().map(|bi| bi.definition.clone()).collect();
    let enrichers: Vec<Box<dyn Enricher>> = if args.no_enrich {
        Vec::new()
    } else {
        vec![Box::new(OpenFigiEnricher::new(env::var("OPENFIGI_API_KEY").ok()))]
    };
    let (summary, ids) = catalog::upsert_catalog(&pool, &defs, &enrichers).await?;
    info!(
        "master: upserted={} skipped_fk={} skipped_expired={} derivatives={} enriched={}",
        summary.upserted, summary.skipped_fk(), summary.skipped_expired, summary.derivatives, summary.enriched
    );

    // 2) Attach the broker routing mapping for every instrument that landed.
    let broker_code = args.broker.code();
    let rows: Vec<BrokerRow> = catalog
        .iter()
        .filter_map(|bi| {
            let key = (bi.definition.symbol.clone(), bi.definition.venue.clone());
            ids.get(&key).map(|&id| BrokerRow::from(id, bi))
        })
        .collect();
    info!("mapping {} instrument(s) to {broker_code}", rows.len());

    let mut tx = pool.begin().await?;
    let mut n = 0usize;
    for chunk in rows.chunks(BATCH) {
        n += bulk_upsert_broker_instrument(&mut tx, broker_code, chunk).await?;
    }
    tx.commit().await?;
    info!("sync-broker done: upserted {n} {broker_code} broker_instrument row(s)");

    // Loud by default. `upsert_catalog` already named the offending venues and
    // currencies; make them consequential so a half-seeded catalog cannot pass for
    // a finished one in a script or CI step.
    let skipped = summary.skipped_fk();
    if skipped > 0 && !args.allow_skips {
        return Err(format!(
            "{skipped} instrument(s) skipped on a missing venue/currency — see the warnings above. \
             Seed the missing rows, or pass --allow-skips to accept a partial catalog."
        )
        .into());
    }
    Ok(())
}

/// Build an `AlpacaAdapter` standalone from env (`ALPACA_ENV` + `ALPACA_{ENV}_API_KEY/SECRET`).
fn build_alpaca() -> Result<AlpacaAdapter, Box<dyn std::error::Error>> {
    let env_name = env::var("ALPACA_ENV").unwrap_or_else(|_| "PAPER".into()).to_uppercase();
    let key = env::var(format!("ALPACA_{env_name}_API_KEY"))
        .map_err(|_| format!("ALPACA_{env_name}_API_KEY must be set"))?;
    let secret = env::var(format!("ALPACA_{env_name}_API_SECRET"))
        .map_err(|_| format!("ALPACA_{env_name}_API_SECRET must be set"))?;
    Ok(AlpacaAdapter::new(key, secret, &env_name))
}

/// Build a `BinanceAdapter` from env. The catalog endpoint (exchangeInfo) is
/// public, but the adapter constructor needs a valid key pair; reuse the server
/// wiring (`BINANCE_{ENV}_API_KEY` + `BINANCE_{ENV}_PRIVATE_KEY_PATH`).
fn build_binance() -> Result<BinanceAdapter, Box<dyn std::error::Error>> {
    let env_name = env::var("BINANCE_ENV").unwrap_or_else(|_| "PAPER".into()).to_uppercase();
    let key = env::var(format!("BINANCE_{env_name}_API_KEY"))
        .map_err(|_| format!("BINANCE_{env_name}_API_KEY must be set"))?;
    let pem_path = env::var(format!("BINANCE_{env_name}_PRIVATE_KEY_PATH"))
        .map_err(|_| format!("BINANCE_{env_name}_PRIVATE_KEY_PATH must be set"))?;
    let pem = std::fs::read_to_string(&pem_path)
        .map_err(|e| format!("reading {pem_path}: {e}"))?;
    BinanceAdapter::new(key, &pem, &env_name).map_err(Into::into)
}

/// A broker mapping row ready to upsert into `broker_instrument`.
struct BrokerRow {
    instrument_id: i64,
    broker_symbol: String,
    broker_exchange: Option<String>,
    native_id: Option<String>,
    is_tradeable: bool,
    min_quantity: Option<f64>,
    max_quantity: Option<f64>,
    min_notional: Option<f64>,
    max_notional: Option<f64>,
}

impl BrokerRow {
    fn from(instrument_id: i64, bi: &BrokerInstrument) -> Self {
        Self {
            instrument_id,
            broker_symbol: bi.broker_symbol.clone(),
            broker_exchange: bi.broker_exchange.clone(),
            native_id: bi.definition.native_id.clone(),
            is_tradeable: bi.is_tradeable,
            min_quantity: bi.min_quantity,
            max_quantity: bi.max_quantity,
            min_notional: bi.min_notional,
            max_notional: bi.max_notional,
        }
    }
}

/// Bulk-upsert `broker_instrument` rows via UNNEST, one row per instrument per
/// broker. Conflict key is (instrument_id, broker_code).
async fn bulk_upsert_broker_instrument(
    tx: &mut Transaction<'_, Postgres>,
    broker_code: &str,
    chunk: &[BrokerRow],
) -> Result<usize, Box<dyn std::error::Error>> {
    let instrument_id: Vec<i64> = chunk.iter().map(|r| r.instrument_id).collect();
    let broker_symbol: Vec<String> = chunk.iter().map(|r| r.broker_symbol.clone()).collect();
    let broker_exchange: Vec<Option<String>> = chunk.iter().map(|r| r.broker_exchange.clone()).collect();
    let native_id: Vec<Option<String>> = chunk.iter().map(|r| r.native_id.clone()).collect();
    let is_tradeable: Vec<bool> = chunk.iter().map(|r| r.is_tradeable).collect();
    let min_quantity: Vec<Option<f64>> = chunk.iter().map(|r| r.min_quantity).collect();
    let max_quantity: Vec<Option<f64>> = chunk.iter().map(|r| r.max_quantity).collect();
    let min_notional: Vec<Option<f64>> = chunk.iter().map(|r| r.min_notional).collect();
    let max_notional: Vec<Option<f64>> = chunk.iter().map(|r| r.max_notional).collect();

    sqlx::query(
        "INSERT INTO broker_instrument \
            (instrument_id, broker_code, broker_symbol, broker_exchange, native_id, \
             is_tradeable, min_quantity, max_quantity, min_notional, max_notional) \
         SELECT t.iid, $1, t.sym, t.exch, t.nid, t.trad, t.minq, t.maxq, t.minn, t.maxn \
         FROM UNNEST($2::bigint[], $3::text[], $4::text[], $5::text[], $6::bool[], \
                     $7::float8[], $8::float8[], $9::float8[], $10::float8[]) \
              AS t(iid, sym, exch, nid, trad, minq, maxq, minn, maxn) \
         ON CONFLICT (instrument_id, broker_code) \
         DO UPDATE SET broker_symbol   = EXCLUDED.broker_symbol, \
                       broker_exchange = EXCLUDED.broker_exchange, \
                       native_id       = EXCLUDED.native_id, \
                       is_tradeable    = EXCLUDED.is_tradeable, \
                       min_quantity    = EXCLUDED.min_quantity, \
                       max_quantity    = EXCLUDED.max_quantity, \
                       min_notional    = EXCLUDED.min_notional, \
                       max_notional    = EXCLUDED.max_notional, \
                       updated_at      = now()",
    )
    .bind(broker_code)
    .bind(&instrument_id)
    .bind(&broker_symbol)
    .bind(&broker_exchange)
    .bind(&native_id)
    .bind(&is_tradeable)
    .bind(&min_quantity)
    .bind(&max_quantity)
    .bind(&min_notional)
    .bind(&max_notional)
    .execute(&mut **tx)
    .await?;

    Ok(chunk.len())
}
