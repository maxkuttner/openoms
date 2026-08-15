//! Instrument expiry lifecycle: derive each dated contract's UTC expiry instant,
//! then retire the ones that have passed.
//!
//! Two problems, one job. The catalog stores `instrument_derivative.expiry_date` as
//! a bare DATE because that is what the source gives it — Alpaca sends
//! `"YYYY-MM-DD"`, which is a *venue-local* calendar date, and an option trades
//! until 16:00 that day, not until midnight. Comparing that date to `current_date`
//! would make the answer depend on the database server's timezone. So the first half
//! of this module resolves the date into a real instant using the venue's calendar,
//! and everything downstream compares instants.
//!
//! The second half is the lifecycle gap that motivated it. [`crate::setup::catalog`]
//! filters already-expired contracts at *ingest*, so nothing expired ever enters the
//! catalog — but nothing ages a row out *afterwards*. An option seeded while live
//! stays `ACTIVE` forever once its expiry passes, and re-running the broker sync does
//! not help: it is an upsert, so a contract that simply stops being listed leaves its
//! stale row untouched. The visible symptom was Databento answering
//! `SymbolResolutionFailed` for a contract the quote feed kept resubscribing.
//!
//! Retiring an instrument is all this does. The existing `status = 'ACTIVE'` filters
//! on the order path ([`crate::handlers`]) and the subscription path
//! ([`crate::quote_feed::load_subscribable`]) then take effect on their own — no
//! expiry-awareness anywhere else.
//!
//! Deliberately *not* here: what happens to an open order or a held position when its
//! contract expires. The broker cancels the order and the custodian resolves the
//! position (worthless, or exercised into the underlying). Both are events the OMS
//! receives — order recon already reconciles to the broker's snapshot — and neither
//! is something it should infer. [`crate::preflight`] reports a position stranded in
//! an expired contract; it does not act on one.

use std::time::Duration;

use sqlx::PgPool;
use tracing::{error, info};

/// How long between sweeps.
///
/// Hourly rather than daily: the work is two `UPDATE`s that match almost nothing on a
/// normal tick, so the only thing a longer period buys is a contract that stays
/// tradeable for most of a day after it stopped existing. Hourly also avoids
/// next-run-at-wall-clock arithmetic — this is a plain interval, and a restart at any
/// hour is equivalent to one at any other.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Fill in (or correct) `expires_at` from the venue calendar. Returns rows written.
///
/// `(expiry_date + close_time) AT TIME ZONE timezone` is evaluated by Postgres, which
/// carries the IANA database and applies the offset in force *on that date* — 16:00
/// New York is 20:00Z in summer and 21:00Z in winter, so a stored offset would be
/// wrong for half the year. Doing it in SQL also keeps the tz database in one place,
/// patched with the server, rather than pinned to a Rust crate version.
///
/// The `IS DISTINCT FROM` guard makes this idempotent *and* self-healing: a normal
/// run writes nothing, while correcting a calendar's close time or timezone re-derives
/// every contract on that venue at the next tick. `IS DISTINCT FROM` rather than `<>`
/// so a row that has never been computed (NULL) also matches.
///
/// A contract whose venue has no calendar row, or whose calendar has no `close_time`,
/// is skipped and keeps a NULL `expires_at` — it can never be swept, so preflight
/// reports it rather than this function inventing an hour for it.
pub async fn recompute_expires_at(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE instrument_derivative d \
         SET    expires_at = (d.expiry_date + c.close_time) AT TIME ZONE c.timezone \
         FROM   instrument i \
         JOIN   venue v    ON v.code = i.venue \
         JOIN   calendar c ON c.venue_id = v.id \
         WHERE  d.instrument_id = i.id \
           AND  d.expiry_date IS NOT NULL \
           AND  c.close_time IS NOT NULL \
           AND  d.expires_at IS DISTINCT FROM \
                (d.expiry_date + c.close_time) AT TIME ZONE c.timezone",
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(n)
}

/// Retire every instrument whose expiry instant has passed. Returns rows retired.
///
/// Only `ACTIVE` rows are touched, so a contract someone deliberately set to `HALTED`
/// or `INACTIVE` keeps that status — the sweep records that an expiry happened, it
/// does not overwrite a judgement about why something was already not trading.
///
/// `expires_at IS NOT NULL` is doing real work: a NULL means the instant could not be
/// derived (no calendar), not that the contract is perpetual. Sweeping on a NULL would
/// be sweeping on an unknown.
pub async fn sweep_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE instrument i \
         SET    status = 'EXPIRED', updated_at = now() \
         FROM   instrument_derivative d \
         WHERE  d.instrument_id = i.id \
           AND  i.status = 'ACTIVE' \
           AND  d.expires_at IS NOT NULL \
           AND  d.expires_at < now()",
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(n)
}

/// One pass: derive instants, then retire what has passed. Returns
/// `(recomputed, expired)`.
///
/// Order matters — a contract seeded minutes ago has no instant yet, and sweeping
/// before deriving would skip it for a full interval. Shared with the admin trigger so
/// running it by hand and running it on the timer cannot drift apart.
pub async fn run_once(pool: &PgPool) -> Result<(u64, u64), sqlx::Error> {
    let recomputed = recompute_expires_at(pool).await?;
    let expired = sweep_expired(pool).await?;
    Ok((recomputed, expired))
}

/// Background task: sweep at boot, then once an interval, forever.
///
/// A failed pass is logged and the loop continues. The alternative — letting the task
/// die on a transient database error — would silently stop expiring instruments for
/// the lifetime of the process, and the symptom would surface much later as a feed
/// subscribing to a contract that no longer exists.
pub async fn run(pool: PgPool) {
    loop {
        match run_once(&pool).await {
            // Quiet on a no-op tick, which is almost every tick.
            Ok((0, 0)) => {}
            Ok((recomputed, expired)) => {
                info!(recomputed, expired, "expiry sweep");
            }
            Err(e) => error!(error = %e, "expiry sweep failed; retrying next interval"),
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}
