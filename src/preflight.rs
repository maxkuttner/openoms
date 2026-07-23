//! Startup checks: refuse to boot broken, and say so loudly when merely degraded.
//!
//! Runs once in `serve()` after the pool connects and before any feed spawns.
//! Database-only by design — it never calls a broker or vendor API, so it stays
//! fast, works with every venue offline, and cannot be the reason startup hangs.
//!
//! Two severities, and the split is deliberate:
//!
//! * **Fatal** — the catalog is empty or its FK targets are missing. Nothing can
//!   work in these states and booting only hides them behind a later, stranger
//!   error.
//! * **Degraded** — a specific held position cannot be priced or cannot be routed.
//!   Logged at ERROR with the symbol, but the server still comes up: a position you
//!   cannot price is exactly the position you need the application running to
//!   manage. Refusing to boot would be the worse failure.

use sqlx::PgPool;
use tracing::{error, info};

/// How many offending symbols to name before truncating. The point is to notice a
/// class of problem, not to dump the catalog into the log.
const SAMPLE: usize = 10;

/// A condition that makes the process unable to do anything useful.
#[derive(Debug, PartialEq, Eq)]
pub struct Fatal(pub String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Run every check. `Err` means do not start.
///
/// `auto_sync_pending` is true when a background broker sync is about to populate an
/// empty catalog (see `setup::bootstrap`); it downgrades an empty `instrument` table
/// from fatal to expected.
pub async fn run(pool: &PgPool, auto_sync_pending: bool) -> Result<(), Fatal> {
    check_catalog(pool, auto_sync_pending).await?;
    report_held(pool).await;
    Ok(())
}

/// Fatal checks: the master catalog and the FK targets it depends on.
///
/// An empty `venue` or `currency` table is the signature of a DB that never got
/// seeded; with bootstrap on these are seeded before we ever get here, so a failure
/// now means `OMS_BOOTSTRAP=off` over an unprepared DB. An empty `instrument` is only
/// fatal when nothing is about to fill it — a pending background sync makes it
/// expected, not broken.
async fn check_catalog(pool: &PgPool, auto_sync_pending: bool) -> Result<(), Fatal> {
    for (table, hint) in [
        ("venue", "run `make db-seed` (or enable bootstrap)"),
        ("currency", "run `make db-seed` (or enable bootstrap)"),
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .map_err(|e| Fatal(format!("preflight: reading {table} failed: {e}")))?;
        if n == 0 {
            return Err(Fatal(format!("{table} is empty — {hint}")));
        }
    }

    let instruments: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument")
        .fetch_one(pool)
        .await
        .map_err(|e| Fatal(format!("preflight: reading instrument failed: {e}")))?;
    if instruments == 0 {
        if auto_sync_pending {
            info!("preflight: catalog empty — a background broker sync will populate it");
        } else {
            return Err(Fatal(
                "instrument is empty — set broker creds (auto-sync), run \
                 `make sync-broker BROKER=alpaca`, or `make db-fixtures` for the SPY-only set"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

/// Degraded checks, per held position. Logs; never fails the boot.
///
/// Answers the two questions that otherwise fail silently at runtime: can we mark
/// this position, and can we close it?
async fn report_held(pool: &PgPool) {
    // position.instrument_id is text holding the numeric instrument.id, so a held
    // row whose instrument is gone shows up as a NULL join rather than an error.
    let held = sqlx::query_as::<_, HeldRow>(
        "SELECT DISTINCT p.instrument_id AS raw_id, i.id, i.symbol, \
                i.instrument_class, i.asset_class, i.venue \
         FROM position p \
         LEFT JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.net_qty <> 0",
    )
    .fetch_all(pool)
    .await;

    let held = match held {
        Ok(h) => h,
        // Not fatal: the catalog checks passed, so this is a transient read
        // failure, and refusing to boot over a report would be perverse.
        Err(e) => {
            error!("preflight: could not read held positions: {e}");
            return;
        }
    };

    if held.is_empty() {
        info!("preflight: no held positions");
        return;
    }

    let orphans: Vec<&str> = held
        .iter()
        .filter(|r| r.id.is_none())
        .map(|r| r.raw_id.as_str())
        .collect();
    if !orphans.is_empty() {
        error!(
            "preflight: {} held position(s) reference a missing instrument: {}",
            orphans.len(),
            sample(&orphans)
        );
    }

    let live: Vec<&HeldRow> = held.iter().filter(|r| r.id.is_some()).collect();

    // Priceable: does any feed's symbology both select this instrument and accept
    // its symbol? Uses the same two functions the live path uses, so this cannot
    // drift from what actually gets subscribed.
    let unpriceable: Vec<&str> = live
        .iter()
        .filter(|r| !crate::feeds::ALL.iter().any(|feed| covers(*feed, r)))
        .filter_map(|r| r.symbol.as_deref())
        .collect();
    if !unpriceable.is_empty() {
        error!(
            "preflight: {} held instrument(s) no feed can price — they will sit unmarked: {}",
            unpriceable.len(),
            sample(&unpriceable)
        );
    }

    // Routable: a held instrument with no broker_instrument row cannot be closed.
    let unroutable = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT i.symbol \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         LEFT JOIN broker_instrument bi ON bi.instrument_id = i.id AND bi.is_tradeable \
         WHERE p.net_qty <> 0 AND bi.instrument_id IS NULL \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await;
    match unroutable {
        Ok(rows) if !rows.is_empty() => {
            let names: Vec<&str> = rows.iter().map(String::as_str).collect();
            error!(
                "preflight: {} held instrument(s) have no tradeable broker mapping — cannot be closed: {}",
                names.len(),
                sample(&names)
            );
        }
        Ok(_) => {}
        Err(e) => error!("preflight: could not check broker routing: {e}"),
    }

    // One line per feed, so the healthy case is a glance rather than an absence.
    for feed in crate::feeds::ALL {
        let n = live.iter().filter(|r| covers(*feed, r)).count();
        info!("preflight: {} covers {} of {} held instrument(s)", feed.code(), n, live.len());
    }
}

/// A held position joined to its instrument. `id`/`symbol`/… are `NULL` when the
/// position references an instrument that no longer exists.
#[derive(sqlx::FromRow)]
struct HeldRow {
    raw_id: String,
    id: Option<i64>,
    symbol: Option<String>,
    instrument_class: Option<String>,
    asset_class: Option<String>,
    venue: Option<String>,
}

/// Whether `feed` would actually subscribe this instrument.
///
/// Mirrors `load_subscribable` exactly: every `Some` field of `candidates()` must
/// match, *and* `to_feed_symbol` must accept the symbol. Checking only some of the
/// filter would over-report coverage and hide the very positions this exists to
/// surface.
fn covers(feed: &dyn dataprovider::FeedSymbology, row: &HeldRow) -> bool {
    let f = feed.candidates();
    let matches = |want: Option<&str>, have: &Option<String>| match (want, have) {
        (None, _) => true,
        (Some(w), Some(h)) => w == h,
        // The filter constrains a column the row has no value for: not a match.
        (Some(_), None) => false,
    };

    matches(f.instrument_class, &row.instrument_class)
        && matches(f.asset_class, &row.asset_class)
        && matches(f.venue, &row.venue)
        && row.symbol.as_deref().is_some_and(|s| feed.to_feed_symbol(s).is_some())
}

/// Render up to [`SAMPLE`] names, noting how many were withheld.
fn sample(names: &[&str]) -> String {
    let shown: Vec<&str> = names.iter().copied().take(SAMPLE).collect();
    let rest = names.len().saturating_sub(shown.len());
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} (+{rest} more)", shown.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_lists_everything_when_short() {
        assert_eq!(sample(&["A", "B"]), "A, B");
    }

    /// A big unpriceable set must not dump the catalog into the log line.
    #[test]
    fn sample_truncates_and_says_how_many_it_hid() {
        let names: Vec<String> = (0..13).map(|i| format!("S{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let out = sample(&refs);
        assert!(out.ends_with("(+3 more)"), "{out}");
        assert_eq!(out.matches(", ").count(), SAMPLE - 1);
    }

    #[test]
    fn sample_of_nothing_is_empty() {
        assert_eq!(sample(&[]), "");
    }

    fn row(symbol: &str, class: &str, asset: &str, venue: &str) -> HeldRow {
        HeldRow {
            raw_id: "1".into(),
            id: Some(1),
            symbol: Some(symbol.into()),
            instrument_class: Some(class.into()),
            asset_class: Some(asset.into()),
            venue: Some(venue.into()),
        }
    }

    /// The case already live in the fixture: SPY equity on ARCX. It is *not*
    /// priceable by the OPRA feed — that feed carries options only — so preflight
    /// must report it rather than let it sit silently unmarked.
    #[test]
    fn equity_is_not_covered_by_the_opra_feed() {
        let held = row("SPY", "SPOT", "EQUITY", "ARCX");
        assert!(!covers(&crate::opra_stream::DatabentoOpraFeed, &held));
        assert!(!crate::feeds::ALL.iter().any(|f| covers(*f, &held)));
    }

    #[test]
    fn opra_feed_covers_a_held_option() {
        let held = row("SPY260724P00739000", "OPTION", "EQUITY", "OPRA");
        assert!(covers(&crate::opra_stream::DatabentoOpraFeed, &held));
    }

    /// Bybit leaves `venue` open, so it prices the pair wherever it is listed —
    /// including the Binance-seeded row. Binance does not: its filter pins the venue.
    #[test]
    fn open_venue_filter_covers_another_venues_listing() {
        let held = row("BTCUSDT", "SPOT", "CRYPTO", "BINANCE");
        assert!(covers(&crate::bybit_feed::BybitFeed, &held));
        assert!(covers(&crate::binance_feed::BinanceFeed, &held));

        let elsewhere = row("BTCUSDT", "SPOT", "CRYPTO", "XNAS");
        assert!(covers(&crate::bybit_feed::BybitFeed, &elsewhere));
        assert!(!covers(&crate::binance_feed::BinanceFeed, &elsewhere));
    }

    /// A symbol inside `candidates` that the symbology still declines is not
    /// covered — the filter alone is not enough.
    #[test]
    fn declined_symbol_is_not_covered_despite_matching_the_filter() {
        let malformed = row("SPY", "OPTION", "EQUITY", "OPRA");
        assert!(!covers(&crate::opra_stream::DatabentoOpraFeed, &malformed));
    }
}
