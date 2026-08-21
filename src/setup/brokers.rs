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
    /// This is now the single source of truth for "can we sync this broker":
    /// `run` (below) sources `build_alpaca`/`build_binance`'s credential from the
    /// same store, so cred *detection* (`bootstrap::will_sync_on_boot`/
    /// `spawn_sync`) and cred *use* (`run`) can never disagree — mirroring the
    /// guarantee this method's doc comment made before the credential store
    /// existed, when both sides read the environment instead.
    pub fn has_creds(self, connections: &[Connection<BrokerCredentials>]) -> bool {
        connections
            .iter()
            .any(|c| c.code == self.connection_code() && matches!(c.credentials, CredentialState::Configured(_)))
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

    // Credentials come from the store — the same load `serve()` and
    // `config import-env` use — so this command and boot-time adapter
    // registration can never disagree about what a broker is configured with.
    // Same three-way key handling as `serve()`: absent-and-nothing-stored is a
    // normal fresh install, absent-with-credentials-stored is fatal, and an
    // invalid key is fatal.
    let master = match crate::config::master_key(crate::config::load()) {
        Some(Ok(k)) => Some(k),
        Some(Err(e)) => return Err(format!("invalid master key: {e}").into()),
        None => None,
    };
    if master.is_none() {
        // Fail safe, not fail open: a query failure must not silently disarm
        // this the way `.unwrap_or(false)` would — see the identical reasoning
        // in `serve()`.
        let might_be_stored = match crate::credentials::any_credentials_stored(&pool).await {
            Ok(b) => b,
            Err(e) => {
                warn!("could not determine whether credentials are stored, assuming they may be: {e}");
                true
            }
        };
        if might_be_stored {
            return Err(
                "credentials are stored but no master key is configured \
                 (set oms.master_key in oms.toml, or OMS_MASTER_KEY)"
                    .into(),
            );
        }
    }

    let code = args.broker.connection_code();
    let connections = crate::credentials::load_brokers(&pool, master.as_ref()).await?;
    let creds = match connections.into_iter().find(|c| c.code == code) {
        Some(Connection { credentials: CredentialState::Configured(creds), .. }) => creds,
        Some(Connection { credentials: CredentialState::Unconfigured, .. }) => {
            return Err(format!(
                "{code}: no credentials stored — run `oms config import-env` (with the \
                 relevant env vars set) or configure it, then retry"
            )
            .into());
        }
        Some(Connection { credentials: CredentialState::Error(e), .. }) => {
            return Err(format!("{code}: credentials unusable: {e}").into());
        }
        None => {
            return Err(format!(
                "{code}: no broker_connection row — start the server once to create it \
                 (ensure_broker_connections runs at boot), then retry"
            )
            .into());
        }
    };

    let underlyings: Vec<String> = args
        .underlyings
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();

    let provider: Box<dyn InstrumentProvider> = match args.broker {
        Broker::Alpaca => Box::new(build_alpaca(&creds)?),
        Broker::Binance => Box::new(build_binance(&creds)?),
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

/// Build an `AlpacaAdapter` from a store credential. `ALPACA_ENV` still selects
/// *which* environment to sync (PAPER vs LIVE) — that is a mode selector, not a
/// secret — but the key/secret themselves come from `creds`, not the environment.
fn build_alpaca(creds: &BrokerCredentials) -> Result<AlpacaAdapter, Box<dyn std::error::Error>> {
    match creds {
        BrokerCredentials::Alpaca { key, secret } => {
            Ok(AlpacaAdapter::new(key.clone(), secret.clone(), &alpaca_env()))
        }
        // `run` looked this credential up by `Broker::Alpaca.connection_code()`,
        // so any other variant here is a programming error (a code/kind
        // mismatch in the store), not something an operator can hit.
        other => Err(format!("alpaca-* credential is not an Alpaca credential: {other:?}").into()),
    }
}

/// Build a `BinanceAdapter` from a store credential. The catalog endpoint
/// (exchangeInfo) is public, but the adapter constructor needs a valid key pair
/// regardless — `private_key` is PEM contents already (no file read needed).
fn build_binance(creds: &BrokerCredentials) -> Result<BinanceAdapter, Box<dyn std::error::Error>> {
    match creds {
        BrokerCredentials::BinanceFix { api_key, private_key, .. } => {
            BinanceAdapter::new(api_key.clone(), private_key, &binance_env()).map_err(Into::into)
        }
        other => Err(format!("binance-* credential is not a Binance credential: {other:?}").into()),
    }
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

    /// `has_creds`/`brokers_with_creds` derive the connection code from
    /// `ALPACA_ENV`/`BINANCE_ENV` (via `connection_code()`), so a hardcoded
    /// `"alpaca-paper"`/`"binance-paper"` in a test only holds while those vars
    /// are unset or `PAPER` — an ambient `ALPACA_ENV=LIVE` in the shell (or a
    /// prior test in the same binary) would silently break the assertion. These
    /// tests mutate process env, so they must not run concurrently with anything
    /// else reading the same keys — serialized by `ENV_LOCK`, same pattern as
    /// `setup::import_env::tests`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        std::env::remove_var("ALPACA_ENV");
        std::env::remove_var("BINANCE_ENV");
    }

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
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let alpaca_code = Broker::Alpaca.connection_code();
        let connections = vec![connection(&alpaca_code, CredentialState::Configured(alpaca_cred()))];
        let alpaca_has = Broker::Alpaca.has_creds(&connections);
        let binance_has = Broker::Binance.has_creds(&connections);
        clear();

        assert!(alpaca_has);
        assert!(!binance_has);
    }

    /// `Unconfigured` and `Error` are both "not usable" — neither counts as having
    /// credentials, only `Configured` does.
    #[test]
    fn has_creds_is_false_for_unconfigured_and_error_rows() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let connections = vec![
            connection(&Broker::Alpaca.connection_code(), CredentialState::Unconfigured),
            connection(&Broker::Binance.connection_code(), CredentialState::Error("bad key".into())),
        ];
        let alpaca_has = Broker::Alpaca.has_creds(&connections);
        let binance_has = Broker::Binance.has_creds(&connections);
        clear();

        assert!(!alpaca_has);
        assert!(!binance_has);
    }

    /// A code that never appears in the list (no row at all) is indistinguishable
    /// from Unconfigured — no row means no credential either.
    #[test]
    fn has_creds_is_false_when_no_row_exists_for_the_code() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let alpaca_has = Broker::Alpaca.has_creds(&[]);
        clear();

        assert!(!alpaca_has);
    }

    #[test]
    fn brokers_with_creds_returns_only_the_configured_subset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let connections = vec![
            connection(&Broker::Alpaca.connection_code(), CredentialState::Configured(alpaca_cred())),
            connection(&Broker::Binance.connection_code(), CredentialState::Unconfigured),
        ];
        let result = brokers_with_creds(&connections);
        clear();

        assert_eq!(result, vec![Broker::Alpaca]);
    }
}
