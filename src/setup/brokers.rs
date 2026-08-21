//! `oms setup sync-broker` — the instrument seeding path.
//!
//! Broker-first: an adapter's [`InstrumentProvider`] enumerates the
//! broker's tradeable catalog; we create the master `public.instrument` (+
//! `instrument_derivative`) rows and the `public.broker_instrument` routing mapping
//! in one pass. The broker is the authoritative source of the instrument — there is
//! no separate dataset catalog and no priceable-but-not-tradeable path.
//!
//! Runs as the ordinary runtime role (`oms`), which holds write on the master
//! catalog and both mapping tables (see `db/access/ods.sql`).

use std::env;

use clap::{Args as ClapArgs, ValueEnum};
use dataprovider::{Enricher, InstrumentDef, OpenFigiEnricher};
use sqlx::{PgPool, Postgres, Transaction};
use tracing::{info, warn};

use crate::adapters::alpaca::AlpacaAdapter;
use crate::adapters::binance::BinanceAdapter;
use crate::adapters::{BrokerInstrument, InstrumentProvider};
use crate::credentials::{BrokerCredentials, Connection, CredentialState};
use crate::setup::catalog;

const BATCH: usize = 4000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Broker {
    Alpaca,
    Binance,
}

impl Broker {
    /// Every broker the app can sync — the set boot-time auto-sync iterates.
    pub const ALL: &'static [Broker] = &[Broker::Alpaca, Broker::Binance];

    pub fn code(self) -> &'static str {
        match self {
            Broker::Alpaca => "ALPACA",
            Broker::Binance => "BINANCE",
        }
    }

    /// The configured environment for this broker (`PAPER` | `LIVE`), from
    /// `{BROKER}_ENV`, defaulting to `PAPER`. Together with [`code`](Self::code) this
    /// is the `(broker_code, environment)` pair a `broker_connection` routes on.
    pub fn environment(self) -> String {
        match self {
            Broker::Alpaca => alpaca_env(),
            Broker::Binance => binance_env(),
        }
    }

    /// The conventional `broker_connection.code` for this broker+env, e.g.
    /// `alpaca-paper`.
    pub fn connection_code(self) -> String {
        format!("{}-{}", self.code().to_lowercase(), self.environment().to_lowercase())
    }

    /// Whether this broker has a usable credential in the store — its
    /// conventional `broker_connection.code` (e.g. "alpaca-paper") appears in
    /// `connections` with a `Configured` credential.
    ///
    /// This governs `bootstrap::will_sync_on_boot`/`spawn_sync`: whether the
    /// boot-time catalog auto-sync should run for this broker. It is distinct
    /// from [`has_env_creds`](Self::has_env_creds) — see that method's doc
    /// comment for why the two can (narrowly, and non-silently) disagree.
    pub fn has_creds(self, connections: &[Connection<BrokerCredentials>]) -> bool {
        connections
            .iter()
            .any(|c| c.code == self.connection_code() && matches!(c.credentials, CredentialState::Configured(_)))
    }

    /// Whether this broker's credentials are present in the *environment*.
    ///
    /// Kept for `bootstrap::sync_all_brokers`'s own use: `build_alpaca` /
    /// `build_binance` below still construct their adapter directly from the
    /// environment — that code path predates the credential store and Task 7
    /// (boot-time adapter registration) deliberately does not touch it — so the
    /// background catalog sync has to keep asking the question `build_alpaca` /
    /// `build_binance` will actually answer. A broker credentialed only in the
    /// store (no matching env vars) passes [`has_creds`](Self::has_creds) but
    /// fails this — `sync_all_brokers` reports that gap explicitly rather than
    /// silently doing nothing.
    pub(crate) fn has_env_creds(self) -> bool {
        let set = |k: &str| env::var(k).is_ok_and(|v| !v.is_empty());
        match self {
            Broker::Alpaca => {
                let e = alpaca_env();
                set(&format!("ALPACA_{e}_API_KEY")) && set(&format!("ALPACA_{e}_API_SECRET"))
            }
            Broker::Binance => {
                let e = binance_env();
                set(&format!("BINANCE_{e}_API_KEY")) && set(&format!("BINANCE_{e}_PRIVATE_KEY_PATH"))
            }
        }
    }
}

/// Every broker in `Broker::ALL` whose store credential is `Configured`.
/// Computed once by `serve()` and reused for both `will_sync_on_boot` and
/// `spawn_sync`, so the two cannot disagree about which brokers are eligible.
pub fn brokers_with_creds(connections: &[Connection<BrokerCredentials>]) -> Vec<Broker> {
    Broker::ALL.iter().copied().filter(|b| b.has_creds(connections)).collect()
}

fn alpaca_env() -> String {
    env::var("ALPACA_ENV").unwrap_or_else(|_| "PAPER".into()).to_uppercase()
}

fn binance_env() -> String {
    env::var("BINANCE_ENV").unwrap_or_else(|_| "PAPER".into()).to_uppercase()
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
        "master: upserted={} skipped_fk={} skipped_expired={} derivatives={} dated={} enriched={}",
        summary.upserted,
        summary.skipped_fk(),
        summary.skipped_expired,
        summary.derivatives,
        summary.dated,
        summary.enriched
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
    let env_name = alpaca_env();
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
    let env_name = binance_env();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(code: &str, credentials: CredentialState<BrokerCredentials>) -> Connection<BrokerCredentials> {
        Connection {
            code: code.to_string(),
            kind: "TEST".to_string(),
            environment: Some("PAPER".to_string()),
            status: "ACTIVE".to_string(),
            credentials,
            credentials_updated_at: None,
        }
    }

    fn alpaca_cred() -> BrokerCredentials {
        BrokerCredentials::Alpaca { key: "k".into(), secret: "s".into() }
    }

    /// `has_creds` must key off the connection code, not just "is anything
    /// Configured somewhere in the list" — a Binance row must not make Alpaca
    /// look credentialed.
    #[test]
    fn has_creds_is_true_only_for_a_configured_row_with_the_matching_code() {
        let connections = vec![connection("alpaca-paper", CredentialState::Configured(alpaca_cred()))];
        assert!(Broker::Alpaca.has_creds(&connections));
        assert!(!Broker::Binance.has_creds(&connections));
    }

    /// `Unconfigured` and `Error` are both "not usable" — neither counts as having
    /// credentials, only `Configured` does.
    #[test]
    fn has_creds_is_false_for_unconfigured_and_error_rows() {
        let connections = vec![
            connection("alpaca-paper", CredentialState::Unconfigured),
            connection("binance-paper", CredentialState::Error("bad key".into())),
        ];
        assert!(!Broker::Alpaca.has_creds(&connections));
        assert!(!Broker::Binance.has_creds(&connections));
    }

    /// A code that never appears in the list (no row at all) is indistinguishable
    /// from Unconfigured — no row means no credential either.
    #[test]
    fn has_creds_is_false_when_no_row_exists_for_the_code() {
        assert!(!Broker::Alpaca.has_creds(&[]));
    }

    #[test]
    fn brokers_with_creds_returns_only_the_configured_subset() {
        let connections = vec![
            connection("alpaca-paper", CredentialState::Configured(alpaca_cred())),
            connection("binance-paper", CredentialState::Unconfigured),
        ];
        assert_eq!(brokers_with_creds(&connections), vec![Broker::Alpaca]);
    }
}
