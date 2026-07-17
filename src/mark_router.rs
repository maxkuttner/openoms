//! Routes normalized quotes from every feed into the [`MarkStore`], choosing which
//! source wins when more than one can price an instrument.
//!
//! This is the only writer, so "which source is authoritative" is decided in one
//! place rather than by whichever feed ticked last. The policy is **ranked primary
//! with staleness-driven failover**: the best-ranked source that is still producing
//! quotes owns the mark; when it goes quiet, the next one takes over and the switch
//! is logged.
//!
//! Staleness — rather than a health/disconnect signal — is the trigger because it
//! catches the failure that a connection state cannot: a socket that is up and
//! subscribed but silently no longer delivering. A feed that stops ticking is
//! useless whether or not its TCP connection admits it.
//!
//! This is not A/B line arbitration. That is per-message sequence-number
//! reconciliation between two redundant lines of the *same* feed, at microsecond
//! granularity. This is cross-vendor, at seconds, between genuinely different books
//! — Binance and Bybit quote the same pair but are not the same market.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use dataprovider::Quote;
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::marks::MarkStore;

/// How long a source may go quiet before a worse-ranked source may take over.
///
/// Must exceed the natural gap between quotes on the quietest instrument a feed
/// covers, or the mark will flap between sources on an idle book. Safe today: the
/// only multi-source class is crypto SPOT, which ticks continuously; options have a
/// single source, so nothing can preempt them however long they sit quiet.
const STALE_AFTER_SECS: i64 = 10;

/// Rank applied to a source with no policy row. Higher than any configured rank, so
/// an unconfigured source is usable but never preferred over a configured one.
const UNRANKED: i32 = 1000;

/// Who currently owns an instrument's mark.
struct Active {
    source: &'static str,
    rank: i32,
    at: DateTime<Utc>,
}

/// Drain quotes into the store until every feed has dropped its sender.
pub async fn run(mut quotes: mpsc::Receiver<Quote>, marks: MarkStore, pool: PgPool) {
    let policy = match load_policy(&pool).await {
        Ok(p) => {
            info!(rules = p.len(), "mark router: loaded source policy");
            p
        }
        Err(e) => {
            // Not fatal: every source falls back to UNRANKED, so marks still flow
            // first-come. Degrading to "some mark" beats no marks at all.
            warn!(error = %e, "mark router: policy load failed, all sources unranked");
            HashMap::new()
        }
    };

    let mut classes: HashMap<i64, String> = HashMap::new();
    let mut active: HashMap<i64, Active> = HashMap::new();
    let mut count: u64 = 0;

    info!("mark router: started");

    while let Some(q) = quotes.recv().await {
        if !is_usable(q.bid, q.ask) {
            debug!(
                instrument_id = q.instrument_id,
                source = q.source_code,
                bid = q.bid,
                ask = q.ask,
                "mark router: skipping unusable quote"
            );
            continue;
        }

        // instrument_class is immutable, so cache it forever after one lookup.
        let class = match class_of(&pool, &mut classes, q.instrument_id).await {
            Some(c) => c,
            None => continue,
        };
        let rank = *policy.get(&(q.source_code.to_string(), class)).unwrap_or(&UNRANKED);

        if !accept(&mut active, &q, rank) {
            continue;
        }

        marks.set(q.instrument_id, q.bid, q.ask);
        count += 1;
        if count % 10_000 == 0 {
            debug!(quotes = count, "mark router: throughput");
        }
    }

    info!(quotes = count, "mark router: all feeds closed, stopping");
}

/// Decide whether this quote may set the mark, updating ownership as a side effect.
///
/// Logs every ownership change at info: a silent failover is one you cannot
/// diagnose afterwards, and "why is the mark 0.15% off" is exactly the question
/// this answers.
fn accept(active: &mut HashMap<i64, Active>, q: &Quote, rank: i32) -> bool {
    match active.get(&q.instrument_id) {
        None => {
            info!(
                instrument_id = q.instrument_id,
                source = q.source_code,
                rank,
                "mark router: source active"
            );
            active.insert(
                q.instrument_id,
                Active { source: q.source_code, rank, at: q.ts_recv },
            );
            true
        }
        Some(cur) if cur.source == q.source_code => {
            // Incumbent: refresh its freshness clock.
            active.insert(
                q.instrument_id,
                Active { source: q.source_code, rank, at: q.ts_recv },
            );
            true
        }
        Some(cur) if rank < cur.rank => {
            // A better source appeared (or recovered) — take over immediately.
            info!(
                instrument_id = q.instrument_id,
                from = cur.source,
                to = q.source_code,
                rank,
                "mark router: promoting better-ranked source"
            );
            active.insert(
                q.instrument_id,
                Active { source: q.source_code, rank, at: q.ts_recv },
            );
            true
        }
        Some(cur) => {
            // Worse or equal rank: only usable if the incumbent has gone quiet.
            let quiet_for = (q.ts_recv - cur.at).num_seconds();
            if quiet_for >= STALE_AFTER_SECS {
                warn!(
                    instrument_id = q.instrument_id,
                    from = cur.source,
                    to = q.source_code,
                    quiet_for_secs = quiet_for,
                    "mark router: primary stale, failing over"
                );
                active.insert(
                    q.instrument_id,
                    Active { source: q.source_code, rank, at: q.ts_recv },
                );
                true
            } else {
                false
            }
        }
    }
}

/// Can this book produce a meaningful mid?
///
/// A zero *bid* is a real price, not a missing side: a deep-OTM option quoted
/// 0.00 x 0.05 is genuinely worth about 0.025, and rejecting it would report "no
/// data" for a contract the market is actively quoting — indistinguishable from
/// having no feed at all. The missing-side sentinel is the vendor's undefined
/// price, which feeds filter out before emitting.
///
/// Genuinely unusable: no offer at all, a negative price, or a crossed book
/// (bid > ask), which means the two sides were read from inconsistent states.
fn is_usable(bid: f64, ask: f64) -> bool {
    ask > 0.0 && bid >= 0.0 && bid <= ask
}

async fn class_of(
    pool: &PgPool,
    cache: &mut HashMap<i64, String>,
    instrument_id: i64,
) -> Option<String> {
    if let Some(c) = cache.get(&instrument_id) {
        return Some(c.clone());
    }
    let row = sqlx::query("SELECT instrument_class FROM instrument WHERE id = $1")
        .bind(instrument_id)
        .fetch_optional(pool)
        .await
        .ok()??;
    let class: String = row.get("instrument_class");
    cache.insert(instrument_id, class.clone());
    Some(class)
}

async fn load_policy(pool: &PgPool) -> Result<HashMap<(String, String), i32>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT source_code, instrument_class, rank \
         FROM oms.provider_feed_policy WHERE enabled",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            (
                (r.get::<String, _>("source_code"), r.get::<String, _>("instrument_class")),
                r.get::<i32, _>("rank"),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(source: &'static str, at: DateTime<Utc>) -> Quote {
        Quote {
            instrument_id: 1,
            bid: 10.0,
            ask: 10.1,
            bid_size: 1,
            ask_size: 1,
            ts_recv: at,
            source_code: source,
        }
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn first_source_wins_the_instrument() {
        let mut a = HashMap::new();
        assert!(accept(&mut a, &quote("BINANCE", t(0)), 10));
    }

    #[test]
    fn worse_ranked_source_is_ignored_while_primary_is_fresh() {
        let mut a = HashMap::new();
        assert!(accept(&mut a, &quote("BINANCE", t(0)), 10));
        // Bybit ticks 1s later; Binance is healthy, so Bybit must not touch the mark.
        assert!(!accept(&mut a, &quote("BYBIT", t(1)), 20));
    }

    #[test]
    fn worse_ranked_source_takes_over_once_primary_goes_quiet() {
        let mut a = HashMap::new();
        assert!(accept(&mut a, &quote("BINANCE", t(0)), 10));
        assert!(!accept(&mut a, &quote("BYBIT", t(5)), 20));
        // Binance silent for STALE_AFTER_SECS: Bybit is now the best live source.
        assert!(accept(&mut a, &quote("BYBIT", t(STALE_AFTER_SECS)), 20));
    }

    #[test]
    fn primary_reclaims_immediately_when_it_returns() {
        let mut a = HashMap::new();
        assert!(accept(&mut a, &quote("BINANCE", t(0)), 10));
        assert!(accept(&mut a, &quote("BYBIT", t(20)), 20)); // failed over
        // No waiting: a better-ranked source preempts as soon as it ticks again.
        assert!(accept(&mut a, &quote("BINANCE", t(21)), 10));
        assert!(!accept(&mut a, &quote("BYBIT", t(22)), 20));
    }

    #[test]
    fn incumbent_keeps_refreshing_its_own_clock() {
        let mut a = HashMap::new();
        assert!(accept(&mut a, &quote("BINANCE", t(0)), 10));
        for s in 1..30 {
            assert!(accept(&mut a, &quote("BINANCE", t(s)), 10));
        }
        // Never went stale, so the fallback still cannot preempt.
        assert!(!accept(&mut a, &quote("BYBIT", t(30)), 20));
    }

    #[test]
    fn zero_bid_is_a_real_price() {
        assert!(is_usable(0.0, 0.05));
    }

    #[test]
    fn normal_two_sided_book_is_usable() {
        assert!(is_usable(2.28, 2.29));
    }

    #[test]
    fn locked_book_is_usable() {
        assert!(is_usable(2.30, 2.30));
    }

    #[test]
    fn no_offer_is_not_a_mark() {
        assert!(!is_usable(0.0, 0.0));
        assert!(!is_usable(1.0, 0.0));
    }

    #[test]
    fn crossed_book_is_rejected() {
        assert!(!is_usable(2.50, 2.40));
    }

    #[test]
    fn negative_price_is_rejected() {
        assert!(!is_usable(-1.0, 2.0));
    }
}
