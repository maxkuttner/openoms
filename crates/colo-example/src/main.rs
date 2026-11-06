//! Reference co-located market maker. Quotes one bid and one ask around the
//! mid on one Binance pair, every order through the risk library, and only
//! while the control link's dead man's switch allows it.
//!
//! colo-example --link wss://HOST:PORT --token T --ca PEM --symbol BTCUSDT
//! (Binance key from BINANCE_API_KEY / BINANCE_PRIVATE_KEY; testnet unless --live)

mod quote;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use colo_link::client::{DeadMan, Link, LinkConfig};
use colo_link::proto::{FromStrategy, ToStrategy};
use colo_risk::{Limits, OrderIntent, RiskBook};
use dataprovider::{LiveQuoteFeed, NoFeedHealth};
use gateway::adapter::{BrokerAdapter, BrokerOrderRequest};
use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::connectors::binance::feed::BinanceFeed;

#[derive(Parser)]
struct Args {
    /// The OMS's control link, `wss://host:port`.
    #[arg(long)]
    link: String,
    /// The strategy's trading token.
    #[arg(long)]
    token: String,
    /// The OMS's certificate, when it is self-signed.
    #[arg(long)]
    ca: Option<std::path::PathBuf>,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "USDT")]
    quote_currency: String,
    #[arg(long, default_value_t = 20.0)]
    half_spread_bps: f64,
    #[arg(long, default_value_t = 0.0002)]
    qty: f64,
    #[arg(long, default_value_t = 0.01)]
    tick: f64,
    #[arg(long, default_value = "mm1-q")]
    tag: String,
    /// Seconds without the OMS before quoting stops.
    #[arg(long, default_value_t = 5)]
    dead_man_secs: u64,
    #[arg(long)]
    live: bool,
    /// Keep quoting after a kill: to watch the OMS cancel on sight.
    #[arg(long)]
    ignore_kill: bool,
}

/// Mid prices from Binance's public feed.
fn spawn_mid_feed(symbol: &str) -> watch::Receiver<Option<f64>> {
    let (tx, rx) = watch::channel(None);
    let symbols: HashMap<String, i64> = HashMap::from([(symbol.to_lowercase(), 1)]);
    tokio::spawn(async move {
        loop {
            let (quote_tx, mut quote_rx) = mpsc::channel(256);
            let (_add_tx, mut add_rx) = mpsc::channel(1);
            let symbols = symbols.clone();
            let session = tokio::spawn(async move { BinanceFeed.run_session(symbols, &quote_tx, &mut add_rx, &NoFeedHealth).await });
            while let Some(q) = quote_rx.recv().await {
                let _ = tx.send(Some(q.mid()));
            }
            let _ = session.await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    rx
}

async fn cancel_all(adapter: &BinanceAdapter, symbol: &str, working: &mut Vec<String>) {
    for id in working.drain(..) {
        if let Err(e) = adapter.cancel_order(&id, symbol).await {
            warn!(order = %id, error = %e, "cancel failed");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new("info")).init();
    let args = Args::parse();
    let api_key = std::env::var("BINANCE_API_KEY")?;
    let private_key = std::env::var("BINANCE_PRIVATE_KEY")?;
    let adapter = Arc::new(BinanceAdapter::new(api_key, &private_key, if args.live { "LIVE" } else { "PAPER" })?);
    let ca_pem = args.ca.as_ref().map(std::fs::read).transpose()?;
    let (link, mut control) = Link::spawn(LinkConfig { url: args.link.clone(), token: args.token.clone(), ca_pem });
    let mut mid = spawn_mid_feed(&args.symbol);

    let mut dead = DeadMan::new(Duration::from_secs(args.dead_man_secs), Instant::now());
    let mut risk = RiskBook::new(Limits::default(), 0);
    let mut working: Vec<String> = Vec::new();
    let mut n = 0u64;
    let mut requote = tokio::time::interval(Duration::from_secs(2));
    let mut was_trading = false;

    loop {
        tokio::select! {
            msg = control.recv() => {
                let Some(msg) = msg else { break };
                dead.heard(Instant::now());
                dead.apply(&msg);
                match msg {
                    ToStrategy::State { limits, limits_version, positions, .. } => {
                        risk.set_limits(limits, limits_version);
                        risk.set_positions(positions.into_iter().map(|p| (p.symbol, p.qty)));
                        link.send(FromStrategy::AckLimits { limits_version }).await;
                    }
                    ToStrategy::Limits { limits, limits_version } => {
                        risk.set_limits(limits, limits_version);
                        link.send(FromStrategy::AckLimits { limits_version }).await;
                    }
                    ToStrategy::Positions { positions } => risk.set_positions(positions.into_iter().map(|p| (p.symbol, p.qty))),
                    ToStrategy::Kill => {
                        info!("killed by the OMS");
                        if !args.ignore_kill {
                            cancel_all(&adapter, &args.symbol, &mut working).await;
                        }
                        link.send(FromStrategy::AckKill).await;
                    }
                    ToStrategy::Resume => info!("resumed by the OMS"),
                    ToStrategy::Heartbeat => {}
                }
            }
            _ = requote.tick() => {
                let trading = args.ignore_kill || dead.may_trade(Instant::now());
                if !trading {
                    if was_trading {
                        warn!("lost the OMS or stopped by it: cancelling quotes");
                        cancel_all(&adapter, &args.symbol, &mut working).await;
                    }
                    was_trading = false;
                    continue;
                }
                was_trading = true;
                let Some(m) = *mid.borrow_and_update() else { continue };
                cancel_all(&adapter, &args.symbol, &mut working).await;
                let (bid, ask) = quote::quotes(m, args.half_spread_bps, args.tick);
                for (buy, price) in [(true, bid), (false, ask)] {
                    let intent = OrderIntent { symbol: &args.symbol, quote_currency: &args.quote_currency, buy, qty: args.qty, price };
                    if let Err(r) = risk.check(&intent, m, Instant::now()) {
                        warn!(code = r.code, message = %r.message, "risk refused a quote");
                        continue;
                    }
                    n += 1;
                    let req = BrokerOrderRequest {
                        order_id: quote::client_id(&args.tag, n),
                        symbol: args.symbol.clone(),
                        native_id: None,
                        quantity: args.qty,
                        side: if buy { "buy" } else { "sell" }.into(),
                        order_type: "limit".into(),
                        time_in_force: "gtc".into(),
                        limit_price: Some(price),
                        external_account_ref: String::new(),
                    };
                    match adapter.submit_order(&req).await {
                        Ok(resp) => working.push(resp.external_order_id),
                        Err(e) => warn!(error = %e, "quote not placed"),
                    }
                }
            }
        }
    }
    Ok(())
}
