//! Databento OPRA live top-of-book → marks store.
//!
//! Subscribes to `cmbp-1` (consolidated top-of-book across OPRA's venues — the
//! NBBO, not a single publisher's book) for the option symbols we currently hold
//! (`position.net_qty <> 0`) and writes each quote's mid inputs into the shared
//! [`crate::marks::MarkStore`], which the `/positions` valuation and
//! market-order pre-trade risk read. Mirrors the reconnect/backoff +
//! `stream_health` badge shape of `binance_stream.rs`.
//!
//! OPRA is enormous, so we subscribe only to the held set. Databento maps each
//! `raw_symbol` (the space-padded OSI, stored verbatim in `instrument.symbol`)
//! to a numeric `instrument_id` via a `SymbolMappingMsg`; quote records are then
//! keyed by that numeric id, so we keep a `dbn_id → our_id` table.

use std::collections::HashMap;

use databento::{
    dbn::{self, Schema, SType, UNDEF_PRICE},
    live::Subscription,
    LiveClient,
};
use sqlx::{PgPool, Row};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::marks::MarkStore;
use crate::stream_health::StreamHandle;

const OPRA_DATASET: &str = "OPRA.PILLAR";

/// `position_changed_rx` is owned here and borrowed into each session: the
/// doorbell outlives reconnects, since it has nothing to do with the Databento
/// session.
pub async fn run(
    pool: PgPool,
    marks: MarkStore,
    health: StreamHandle,
    mut position_changed_rx: mpsc::Receiver<()>,
) {
    info!("starting Databento OPRA marks stream");

    let mut backoff_secs: u64 = 1;
    loop {
        // Set the health status to `connecting`, then reset it to live on `success`.
        health.set_connecting();
        match connect_and_run(&pool, &marks, &health, &mut position_changed_rx).await {
            Ok(()) => {
                warn!("OPRA stream closed, reconnecting in {backoff_secs}s");
                health.set_down("stream closed");
            }
            Err(e) => {
                warn!(error = %e, "OPRA stream error, reconnecting in {backoff_secs}s");
                health.set_down(e.to_string());
            }
        }
        sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn connect_and_run(
    pool: &PgPool,
    marks: &MarkStore,
    health: &StreamHandle,
    position_changed_rx: &mut mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get {osi-symbol: instrument_id} map for currently-held options.
    let mut sym_to_id = load_held_option_symbols(pool).await?;

    // If we have no open positions, sleep until a reconnect re-checks the held set.
    if sym_to_id.is_empty() {
        info!("OPRA stream: no open option positions, nothing to subscribe");
        sleep(Duration::from_secs(60)).await;
        return Ok(());
    }

    // Collect symbol to subscribe to.
    let symbols: Vec<String> = sym_to_id.keys().cloned().collect();
    info!(count = symbols.len(), "OPRA stream: subscribing held options");

    // Connect + subscribe to OPRA live
    // TODO: Add other live data feeds dynamically.
    let mut client = LiveClient::builder()
        .key_from_env()? // DATABENTO_API_KEY
        .dataset(OPRA_DATASET)
        .build()
        .await?;

    client
        .subscribe(
            Subscription::builder()
                .symbols(symbols)
                .schema(Schema::Cmbp1)
                .stype_in(SType::RawSymbol)
                .build(),
        )
        .await?;

    client.start().await?;
    health.set_live();
    info!("OPRA stream: live");

    // Databento's numeric instrument_id is session-scoped, so this map is rebuilt
    // from each session's SymbolMappingMsgs and dies with the session. It has to
    // live here, not in a helper: if it sat inside a future `select!` can drop,
    // losing that branch would take the session's id map with it.
    let mut dbn_to_id: HashMap<u32, i64> = HashMap::new();
    // Symbols held but not yet subscribed, queued by the doorbell branch below.
    let mut pending: Vec<String> = Vec::new();

    loop {
        // Subscribe outside the select: `subscribe` is NOT cancel-safe — a dropped
        // partial send makes the gateway reject it and close the connection.
        if !pending.is_empty() {
            info!(count = pending.len(), "OPRA stream: subscribing newly-held options");
            client
                .subscribe(
                    Subscription::builder()
                        .symbols(std::mem::take(&mut pending))
                        .schema(Schema::Cmbp1)
                        .stype_in(SType::RawSymbol)
                        .build(),
                )
                .await?;
        }

        tokio::select! {
            // Doorbell rang: a fill moved a position. Re-read the held set and diff
            // it — we query the DB rather than trust a payload, which is why the
            // signal carries none.
            Some(()) = position_changed_rx.recv() => {
                for (sym, id) in load_held_option_symbols(pool).await? {
                    if !sym_to_id.contains_key(&sym) {
                        pending.push(sym.clone());
                        sym_to_id.insert(sym, id);
                    }
                }
            }
            // Cancel-safe, so losing this branch just drops it mid-await.
            rec = client.next_record() => {
                let Some(rec) = rec? else { break };
                health.record_event();

                // Symbol mapping: numeric instrument_id ↔ raw OSI. Handled in the
                // loop, not once at startup, so symbols added mid-session map too.
                if let Some(sm) = rec.get::<dbn::SymbolMappingMsg>() {
                    if let Ok(osi) = sm.stype_out_symbol() {
                        if let Some(&our_id) = sym_to_id.get(osi) {
                            dbn_to_id.insert(sm.hd.instrument_id, our_id);
                        }
                    }
                    continue;
                }

                // Consolidated top-of-book: update the mark for the mapped instrument.
                if let Some(quote) = rec.get::<dbn::Cmbp1Msg>() {
                    let level = &quote.levels[0];
                    if let (Some(bid), Some(ask)) = (px(level.bid_px), px(level.ask_px)) {
                        if let Some(&our_id) = dbn_to_id.get(&quote.hd.instrument_id) {
                            marks.set(our_id, bid, ask);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}


/// Fixed-point Databento price → f64, or `None` when undefined.
fn px(v: i64) -> Option<f64> {
    (v != UNDEF_PRICE).then_some(v as f64 / 1e9)
}

/// Load the space-padded OSI → our `instrument.id` map for currently-held options.
async fn load_held_option_symbols(
    pool: &PgPool,
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows = sqlx::query(
        // position.instrument_id is text holding the numeric instrument.id.
        "SELECT i.symbol, i.id \
         FROM position p \
         JOIN instrument i ON i.id::text = p.instrument_id \
         WHERE p.net_qty <> 0 AND i.instrument_class = 'OPTION'",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("symbol"), r.get::<i64, _>("id")))
        .collect())
}
