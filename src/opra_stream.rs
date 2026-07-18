//! Databento OPRA live top-of-book, as a [`LiveQuoteFeed`].
//!
//! Subscribes to `cmbp-1` — the consolidated top-of-book across OPRA's ~16 venues,
//! i.e. the NBBO rather than one publisher's book. (`bbo-1s` is not offered on this
//! dataset; `tcbbo` decodes to the same record but only ticks on trades, which
//! would leave an illiquid contract unmarked for hours.)
//!
//! Everything not specific to Databento's wire format lives elsewhere:
//! [`crate::quote_feed`] decides what to subscribe to and watches for new
//! positions, [`crate::stream_supervisor`] reconnects, and
//! [`crate::mark_router`] decides what becomes a mark.

use std::collections::HashMap;

use databento::{
    dbn::{self, Schema, SType, UNDEF_PRICE},
    live::Subscription,
    LiveClient,
};
use dataprovider::{DataProvider, FeedHealth, LiveQuoteFeed, ProviderError, Quote, SymbolAdds};
use tokio::sync::mpsc;
use tracing::{info, warn};

const OPRA_DATASET: &str = "OPRA.PILLAR";
const SOURCE_CODE: &str = "DATABENTO";

pub struct DatabentoOpraFeed;

impl DataProvider for DatabentoOpraFeed {
    fn code(&self) -> &'static str {
        SOURCE_CODE
    }
}

#[async_trait::async_trait]
impl LiveQuoteFeed for DatabentoOpraFeed {
    /// OPRA.PILLAR is an options dataset. Databento cross-references our equities
    /// too, but this session cannot quote them.
    fn covers(&self) -> &'static [&'static str] {
        &["OPTION"]
    }

    async fn run_session(
        &self,
        symbols: HashMap<String, i64>,
        out: &mpsc::Sender<Quote>,
        add_rx: &mut mpsc::Receiver<SymbolAdds>,
        health: &dyn FeedHealth,
    ) -> Result<(), ProviderError> {
        // Databento's raw_symbol is the space-padded OSI, stored verbatim in
        // instrument.symbol, so the keys here need no transform.
        let mut sym_to_id = symbols;

        let mut client = LiveClient::builder()
            .key_from_env()
            .map_err(|e| ProviderError::Config(e.to_string()))?
            .dataset(OPRA_DATASET)
            .build()
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        subscribe(&mut client, sym_to_id.keys().cloned().collect()).await?;
        client
            .start()
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;
        health.on_connected();
        info!("OPRA feed: live");

        // Databento's numeric instrument_id is scoped to (dataset, day) and rotates,
        // so this map is rebuilt from each session's SymbolMappingMsgs and dies with
        // the session. It must live here, outside any future `select!` can drop —
        // losing it mid-session would silently mis-key every subsequent quote.
        let mut dbn_to_id: HashMap<u32, i64> = HashMap::new();
        let mut pending: Vec<String> = Vec::new();

        loop {
            // Subscribe outside the select: `subscribe` is not cancel-safe — the
            // crate warns a dropped partial send makes the gateway reject it and
            // close the connection.
            if !pending.is_empty() {
                info!(count = pending.len(), "OPRA feed: subscribing new symbols");
                subscribe(&mut client, std::mem::take(&mut pending)).await?;
            }

            tokio::select! {
                adds = add_rx.recv() => match adds {
                    Some(adds) => {
                        pending.extend(adds.keys().cloned());
                        sym_to_id.extend(adds);
                    }
                    // Driver gone; nothing more will be added, but keep streaming.
                    None => std::future::pending::<()>().await,
                },
                // `next_record` is cancel-safe, so losing this branch merely drops it.
                rec = client.next_record() => {
                    let rec = rec.map_err(|e| ProviderError::Request(e.to_string()))?;
                    let Some(rec) = rec else { break };
                    health.on_event();

                    // Symbol mapping: the gateway's numeric id for this session.
                    // Handled in the loop, not once at startup, so symbols added
                    // mid-session get mapped too.
                    if let Some(sm) = rec.get::<dbn::SymbolMappingMsg>() {
                        if let Ok(osi) = sm.stype_out_symbol() {
                            if let Some(&our_id) = sym_to_id.get(osi) {
                                dbn_to_id.insert(sm.hd.instrument_id, our_id);
                            }
                        }
                        continue;
                    }

                    if let Some(q) = rec.get::<dbn::Cmbp1Msg>() {
                        let level = &q.levels[0];
                        let (Some(bid), Some(ask)) = (px(level.bid_px), px(level.ask_px)) else {
                            continue;
                        };
                        let Some(&instrument_id) = dbn_to_id.get(&q.hd.instrument_id) else {
                            continue; // quote for something we never mapped
                        };
                        let quote = Quote {
                            instrument_id,
                            bid,
                            ask,
                            bid_size: level.bid_sz,
                            ask_size: level.ask_sz,
                            ts_recv: chrono::Utc::now(),
                            source_code: SOURCE_CODE,
                        };
                        if out.send(quote).await.is_err() {
                            warn!("OPRA feed: mark router gone, ending session");
                            return Ok(());
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

async fn subscribe(client: &mut LiveClient, symbols: Vec<String>) -> Result<(), ProviderError> {
    client
        .subscribe(
            Subscription::builder()
                .symbols(symbols)
                .schema(Schema::Cmbp1)
                .stype_in(SType::RawSymbol)
                .build(),
        )
        .await
        .map_err(|e| ProviderError::Request(e.to_string()))
}

/// Fixed-point Databento price → f64, or `None` when undefined.
fn px(v: i64) -> Option<f64> {
    (v != UNDEF_PRICE).then_some(v as f64 / 1e9)
}
