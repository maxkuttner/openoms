//! `oms setup map-feed` — map a data feed's own symbols onto seeded instruments.
//!
//! Populates `public.feed_instrument`, the market-data mapping. Feeds are
//! independent of brokers: this maps a feed's symbology onto whatever instruments
//! exist, and it is deliberately 1:n — a crypto feed maps its pair onto the pair on
//! every venue that trades it (e.g. the Bybit feed prices the Binance-seeded
//! BTCUSDT). Idempotent; safe to re-run after seeding more instruments.
//!
//! This module owns the *orchestration* only. The symbology itself — which
//! instruments a feed can price and what it calls them — lives on each feed struct
//! as a [`FeedSymbology`] impl, next to the code that speaks that vendor's protocol
//! (`opra_stream.rs`, `binance_feed.rs`, `bybit_feed.rs`). That keeps a vendor's
//! quirks in one place and makes them unit-testable without a database.

use clap::{Args as ClapArgs, ValueEnum};
use dataprovider::FeedSymbology;
use sqlx::PgPool;
use tracing::{info, warn};

/// Max rows per bulk statement, matching `setup::catalog`.
const BATCH: usize = 4000;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Feed {
    /// Databento OPRA consolidated options NBBO. Maps the space-padded OSI.
    Databento,
    /// Binance spot book ticker. Maps the pair on the Binance venue.
    Binance,
    /// Bybit spot orderbook. Maps the pair onto every crypto instrument for it.
    Bybit,
}

impl Feed {
    /// The feed struct that owns this feed's symbology. Unit structs, so these are
    /// `'static` references to promoted constants — no allocation, no state.
    fn symbology(self) -> &'static dyn FeedSymbology {
        match self {
            Feed::Databento => &crate::opra_stream::DatabentoOpraFeed,
            Feed::Binance => &crate::binance_feed::BinanceFeed,
            Feed::Bybit => &crate::bybit_feed::BybitFeed,
        }
    }
}

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Which feed's symbol mapping to (re)build.
    #[arg(long, value_enum)]
    pub feed: Feed,
    /// Count what would be mapped, but write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&super::database_url()?).await?;
    let feed = args.feed.symbology();
    let feed_code = feed.code();

    // The feed declares which slice of the catalog it can price; push that down into
    // the query rather than scanning every instrument.
    let filter = feed.candidates();
    let mut sql = String::from("SELECT id, symbol FROM instrument WHERE status = 'ACTIVE'");
    let mut binds: Vec<&str> = Vec::new();
    for (column, value) in [
        ("instrument_class", filter.instrument_class),
        ("asset_class", filter.asset_class),
        ("venue", filter.venue),
    ] {
        if let Some(v) = value {
            binds.push(v);
            sql.push_str(&format!(" AND {column} = ${}", binds.len()));
        }
    }

    let mut query = sqlx::query_as::<_, (i64, String)>(&sql);
    for b in &binds {
        query = query.bind(*b);
    }
    let candidates = query.fetch_all(&pool).await?;

    // Translate each master symbol into the feed's own. `None` means the feed
    // declines it — counted, not silently dropped.
    let mut feed_symbols: Vec<String> = Vec::with_capacity(candidates.len());
    let mut instrument_ids: Vec<i64> = Vec::with_capacity(candidates.len());
    let mut declined: Vec<String> = Vec::new();
    for (id, symbol) in candidates {
        match feed.to_feed_symbol(&symbol) {
            Some(feed_symbol) => {
                feed_symbols.push(feed_symbol);
                instrument_ids.push(id);
            }
            None => declined.push(symbol),
        }
    }

    if !declined.is_empty() {
        // Bounded sample: the point is to notice a symbology mismatch, not to dump
        // the catalog.
        let sample: Vec<&str> = declined.iter().take(5).map(String::as_str).collect();
        warn!(
            "{feed_code}: {} candidate(s) declined by symbology, e.g. {sample:?}",
            declined.len()
        );
    }

    if args.dry_run {
        info!(
            "dry run: {feed_code} would map {} instrument(s) ({} declined), no write",
            feed_symbols.len(),
            declined.len()
        );
        return Ok(());
    }

    let mut affected = 0u64;
    for (symbol_chunk, id_chunk) in feed_symbols.chunks(BATCH).zip(instrument_ids.chunks(BATCH)) {
        affected += sqlx::query(
            "INSERT INTO feed_instrument (feed_code, feed_symbol, instrument_id) \
             SELECT $1, t.feed_symbol, t.instrument_id \
             FROM UNNEST($2::text[], $3::bigint[]) AS t(feed_symbol, instrument_id) \
             ON CONFLICT (feed_code, feed_symbol, instrument_id) DO NOTHING",
        )
        .bind(feed_code)
        .bind(symbol_chunk)
        .bind(id_chunk)
        .execute(&pool)
        .await?
        .rows_affected();
    }

    info!(
        "map-feed done: {feed_code} mapped {} instrument(s), {affected} new feed_instrument row(s)",
        feed_symbols.len()
    );
    Ok(())
}
