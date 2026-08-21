//! Databento OPRA live playground — connect, subscribe to a few hard-coded
//! option symbols, and print the raw stream. No DB, no OMS wiring. Meant for
//! poking at the live feed to get a feel for it.
//!
//! `DATABENTO_API_KEY` below is read ONLY by this standalone example binary —
//! the OMS server (`oms`/`serve()`) does not consult it. Since the credential
//! store landed, the running server reads the Databento key from
//! `feed_connection` (via `oms config import-env` or the cockpit), sealed with
//! the master key in `oms.toml`. This variable being set has no effect on it.
//!
//! Run:
//!   export DATABENTO_API_KEY=db-...      # your key — this example only
//!   cargo run --example opra_playground
//!
//! Symbols come from `OPRA_SYMBOLS` (comma-separated) or the `SYMBOLS` default
//! below. These are OSI strings in Databento's space-padded form: 6-char root
//! left-justified + space-padded, then YYMMDD, C/P, strike*1000 zero-padded to 8.
//!
//! The defaults WILL go stale: an expired contract is not in live symbology and
//! the gateway answers `SymbolResolutionFailed` (non-fatal — the rest of the
//! subscription still streams). To refresh, list what is actually listed:
//!
//!   curl -s -u "$DATABENTO_API_KEY:" \
//!     -X POST https://hist.databento.com/v0/timeseries.get_range \
//!     -d dataset=OPRA.PILLAR -d symbols=SPY.OPT -d stype_in=parent \
//!     -d schema=definition -d encoding=csv -d start=$(date -v-1d +%F) \
//!     | cut -d, -f6 | sort -u

use databento::{
    dbn::{self, Record, Schema, SType, UNDEF_PRICE},
    live::Subscription,
    LiveClient,
};

// Space-padded OSI. Root is 6 chars, so "SPY" → "SPY   " (3 trailing spaces).
// ATM straddle on the Sep-2026 monthly; verified listed on 2026-08-13 with SPY
// around 772. Overridable with OPRA_SYMBOLS.
const SYMBOLS: &[&str] = &[
    "SPY   260918C00770000",
    "SPY   260918P00770000",
];

const DATASET: &str = "OPRA.PILLAR";

/// Fixed-point Databento price → f64, or None when undefined (i64::MAX).
fn px(v: i64) -> Option<f64> {
    (v != UNDEF_PRICE).then_some(v as f64 / 1e9)
}

/// Left-justify the root in 6 so compact OSI matches Databento's wire form.
/// Already-padded input is returned unchanged (tail is a fixed 15 chars).
fn pad_osi(s: &str) -> String {
    match s.len().checked_sub(15).filter(|&n| n > 0) {
        Some(split) => {
            let (root, tail) = s.split_at(split);
            format!("{:<6}{tail}", root.trim_end())
        }
        None => s.to_string(),
    }
}

/// Readable symbol for a dbn numeric instrument_id (falls back to the raw id).
fn sym_of(map: &std::collections::HashMap<u32, String>, id: u32) -> String {
    map.get(&id).cloned().unwrap_or_else(|| format!("id={id}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Simple logging so the databento crate's traces show up.
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    println!("connecting to {DATASET} …");
    let mut client = LiveClient::builder()
        .key_from_env()? // reads DATABENTO_API_KEY — this example only, see module doc
        .dataset(DATASET)
        .build()
        .await?;

    // OPRA_SYMBOLS lets a stale default be swapped without a rebuild. Roots are
    // padded here so a hand-typed "SPY260918C00770000" also resolves.
    let symbols: Vec<String> = match std::env::var("OPRA_SYMBOLS") {
        Ok(v) => v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(pad_osi).collect(),
        Err(_) => SYMBOLS.iter().map(|s| s.to_string()).collect(),
    };
    // OPRA is multi-venue; use the consolidated top-of-book (cmbp-1) rather than
    // per-publisher mbp-1. (tcbbo — quote-on-trade — decodes to the same Cmbp1Msg.)
    println!("subscribing {} symbols (cmbp-1):", symbols.len());
    for s in &symbols {
        println!("  [{s}]");
    }

    // OPRA is silent outside 09:30–16:00 ET. Set OPRA_REPLAY_MINS=N to replay the
    // last N minutes of real quotes instead, so the decode path can be exercised
    // off-hours. Unset = live only.
    //
    // Note: cmbp-1 does NOT support live replay ("Live replay not supported for
    // cmbp-1 schema"). tcbbo does, and decodes to the same Cmbp1Msg — so
    // OPRA_SCHEMA=tcbbo + OPRA_REPLAY_MINS exercises the identical decode path.
    let replay_mins: Option<i64> = std::env::var("OPRA_REPLAY_MINS")
        .ok()
        .and_then(|s| s.parse().ok());

    let schema = match std::env::var("OPRA_SCHEMA").as_deref() {
        Ok("tcbbo") => Schema::Tcbbo,
        Ok("mbp-1") => Schema::Mbp1,
        _ => Schema::Cmbp1,
    };
    println!("schema: {schema:?}");

    let sub = match replay_mins {
        Some(mins) => {
            let from = chrono::Utc::now() - chrono::Duration::minutes(mins);
            println!("replaying from {from} ({mins} min back)");
            Subscription::builder()
                .symbols(symbols)
                .schema(schema)
                .stype_in(SType::RawSymbol)
                .start(from)
                .build()
        }
        None => Subscription::builder()
            .symbols(symbols)
            .schema(schema)
            .stype_in(SType::RawSymbol)
            .build(),
    };

    client.subscribe(sub).await?;

    client.start().await?;
    println!("live — waiting for records (Ctrl-C to quit)\n");

    // dbn numeric instrument_id → the raw OSI it maps to, for readable output.
    let mut id_to_sym = std::collections::HashMap::<u32, String>::new();

    while let Some(rec) = client.next_record().await? {
        // Symbol mapping: numeric id ↔ raw symbol. Arrives before the quotes.
        if let Some(sm) = rec.get::<dbn::SymbolMappingMsg>() {
            let sym = sm.stype_out_symbol().unwrap_or("?").to_string();
            println!("MAP  dbn_id={:<8} → [{sym}]", sm.hd.instrument_id);
            id_to_sym.insert(sm.hd.instrument_id, sym);
            continue;
        }

        // cmbp-1 / tcbbo → Cmbp1Msg. Consolidated top-of-book at levels[0].
        if let Some(q) = rec.get::<dbn::Cmbp1Msg>() {
            let level = &q.levels[0];
            let sym = sym_of(&id_to_sym, q.hd.instrument_id);
            match (px(level.bid_px), px(level.ask_px)) {
                (Some(bid), Some(ask)) => {
                    let mid = (bid + ask) / 2.0;
                    println!(
                        "BBO  [{sym}]  bid {bid:>8.2} x {:<4}   ask {ask:>8.2} x {:<4}   mid {mid:>8.2}",
                        level.bid_sz, level.ask_sz,
                    );
                }
                _ => println!("BBO  [{sym}]  (one-sided / empty book)"),
            }
            continue;
        }

        // ohlcv-1s → OhlcvMsg. Only emitted when there's trading.
        if let Some(b) = rec.get::<dbn::OhlcvMsg>() {
            let sym = sym_of(&id_to_sym, b.hd.instrument_id);
            println!(
                "BAR  [{sym}]  o {:.2}  h {:.2}  l {:.2}  c {:.2}  vol {}",
                b.open as f64 / 1e9,
                b.high as f64 / 1e9,
                b.low as f64 / 1e9,
                b.close as f64 / 1e9,
                b.volume,
            );
            continue;
        }

        // System messages: gateway heartbeats + acks. Print the text, not just rtype.
        if let Some(sys) = rec.get::<dbn::SystemMsg>() {
            println!("SYS  {}", sys.msg().unwrap_or("?"));
            continue;
        }

        println!("REC  rtype={}", rec.header().rtype);
    }

    println!("stream closed");
    Ok(())
}
