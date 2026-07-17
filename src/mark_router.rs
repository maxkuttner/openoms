//! Routes normalized quotes from every feed into the [`MarkStore`].
//!
//! This is the only writer. Feeds emit [`Quote`]s onto a channel and never touch
//! the store, so adding a vendor cannot change how marks are decided, and the
//! policy for "which source wins" lives in exactly one place.
//!
//! Today that policy is trivial — one provider covers any given instrument, so
//! every quote is accepted. The ranked primary + failover arbitration (prefer the
//! lowest-ranked healthy source; demote on staleness; log the switch) belongs here
//! when a second feed exists. Building it now would be machinery with nothing to
//! arbitrate: no instrument currently has more than one provider.

use dataprovider::Quote;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::marks::MarkStore;

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

/// Drain quotes into the store until every feed has dropped its sender.
pub async fn run(mut quotes: mpsc::Receiver<Quote>, marks: MarkStore) {
    info!("mark router: started");
    let mut count: u64 = 0;

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

        marks.set(q.instrument_id, q.bid, q.ask);
        count += 1;
        if count % 10_000 == 0 {
            debug!(quotes = count, "mark router: throughput");
        }
    }

    info!(quotes = count, "mark router: all feeds closed, stopping");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bid_is_a_real_price() {
        // A deep-OTM option near expiry: worth ~0.025, not "no data".
        // Rejecting this was a regression that made such contracts unmarkable.
        assert!(is_usable(0.0, 0.05));
    }

    #[test]
    fn normal_two_sided_book_is_usable() {
        assert!(is_usable(2.28, 2.29));
    }

    #[test]
    fn locked_book_is_usable() {
        // bid == ask is unusual but not contradictory; the mid is well defined.
        assert!(is_usable(2.30, 2.30));
    }

    #[test]
    fn no_offer_is_not_a_mark() {
        assert!(!is_usable(0.0, 0.0));
        assert!(!is_usable(1.0, 0.0));
    }

    #[test]
    fn crossed_book_is_rejected() {
        // bid > ask: the sides came from inconsistent states.
        assert!(!is_usable(2.50, 2.40));
    }

    #[test]
    fn negative_price_is_rejected() {
        assert!(!is_usable(-1.0, 2.0));
    }
}
