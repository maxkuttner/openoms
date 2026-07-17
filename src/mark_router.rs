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

/// Drain quotes into the store until every feed has dropped its sender.
pub async fn run(mut quotes: mpsc::Receiver<Quote>, marks: MarkStore) {
    info!("mark router: started");
    let mut count: u64 = 0;

    while let Some(q) = quotes.recv().await {
        // A one-sided book is not a mark: a mid computed from a missing side would
        // be silently wrong, and a stale-but-real mark beats a fabricated one.
        if !(q.bid > 0.0 && q.ask > 0.0) {
            debug!(
                instrument_id = q.instrument_id,
                source = q.source_code,
                "mark router: skipping one-sided quote"
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
