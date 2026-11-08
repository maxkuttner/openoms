//! Reference co-located market maker on the embedded runtime. It receives a v2
//! bootstrap from the OMS, quotes through `Runtime`, and never holds venue or
//! OMS credentials inside the runtime crate.
//!
//! colo-example --oms-url wss://HOST:PORT --deployment UUID --token T \
//!   --account-id UUID --account-ref SUBACCOUNT [--ca PEM] [--live]
//! (Binance key from BINANCE_API_KEY / BINANCE_PRIVATE_KEY; testnet unless --live)

mod quote;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use uuid::Uuid;

use colo_link::client::{V2LinkConfig, V2RuntimeLink};
use colo_link::v2::{RuntimeLink, ToRuntime};
use colo_runtime::{
    AccountIdentity, InstrumentPrices, Intent, OrderType, Runtime, RuntimeConfig, RuntimeState,
    Side, TimeInForce,
};
use dataprovider::{LiveQuoteFeed, NoFeedHealth};
use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::connectors::binance::feed::BinanceFeed;
use gateway::connectors::binance::runtime::BinanceRuntimeVenue;

const MAX_JOURNAL_BYTES: u64 = 1 << 30;
const COMPACT_ABOVE_BYTES: u64 = 512 << 20;
const EVENT_QUEUE: usize = 4096;

#[derive(Parser)]
struct Args {
    /// OMS base URL, e.g. wss://oms-host:9879.
    #[arg(long)]
    oms_url: String,
    /// Deployment UUID registered by the OMS.
    #[arg(long)]
    deployment: Uuid,
    /// The strategy's trading token.
    #[arg(long)]
    token: String,
    /// The OMS's certificate, when it is self-signed.
    #[arg(long)]
    ca: Option<std::path::PathBuf>,
    /// Operator-verified venue account UUID.
    #[arg(long)]
    account_id: Uuid,
    /// Operator-verified venue account reference.
    #[arg(long)]
    account_ref: String,
    /// Deployment epoch.
    #[arg(long, default_value_t = 1)]
    epoch: u64,
    #[arg(long, default_value = "colo-journal")]
    journal_dir: std::path::PathBuf,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value_t = 20.0)]
    half_spread_bps: f64,
    #[arg(long, default_value_t = 0.0002)]
    qty: f64,
    #[arg(long, default_value_t = 0.01)]
    tick: f64,
    #[arg(long)]
    live: bool,
}

fn spawn_mid_feed(symbol: &str) -> watch::Receiver<Option<f64>> {
    let (tx, rx) = watch::channel(None);
    let symbols = quote::feed_symbols(symbol);
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

struct MarketMaker {
    args: Args,
    runtime: Runtime,
    working: Vec<String>,
}

impl MarketMaker {
    async fn may_trade(&self) -> bool {
        self.runtime.is_ready().await
    }

    async fn cancel_all(&mut self) {
        for id in self.working.drain(..) {
            if let Err(e) = self.runtime.cancel(&id).await {
                warn!(order = %id, error = %e, "cancel request failed");
            }
        }
    }

    async fn quote(&mut self, buy: bool, price: f64, mid: f64) {
        if !self.may_trade().await {
            return;
        }
        self.runtime
            .set_price(&self.args.symbol, InstrumentPrices::new(Some(mid), Some(mid), Some(mid), Instant::now()))
            .await;
        let intent = Intent {
            instrument: self.args.symbol.clone(),
            side: if buy { Side::Buy } else { Side::Sell },
            quantity: self.args.qty,
            order_type: OrderType::Limit,
            limit_price: Some(price),
            max_quote_spend: None,
            time_in_force: TimeInForce::Gtc,
        };
        match self.runtime.submit(intent).await {
            Ok(submission) => {
                info!(side = if buy { "buy" } else { "sell" }, price, order = %submission.client_order_id, "quote admitted");
                self.working.push(submission.client_order_id);
            }
            Err(e) => warn!(error = %e, "quote not admitted"),
        }
    }
}

async fn receive_bootstrap(link: &V2RuntimeLink) -> Result<colo_link::v2::Bootstrap, Box<dyn std::error::Error>> {
    let first = tokio::time::timeout(Duration::from_secs(10), link.recv_control())
        .await
        .map_err(|_| "timed out waiting for v2 bootstrap")?
        .ok_or("control link closed before bootstrap")?;
    match first.message {
        ToRuntime::Bootstrap { bootstrap } => Ok(bootstrap),
        other => Err(format!("expected bootstrap, got {other:?}").into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new("info")).init();
    let args = Args::parse();
    let api_key = std::env::var("BINANCE_API_KEY")?;
    let private_key = std::env::var("BINANCE_PRIVATE_KEY")?;
    let environment = if args.live { "LIVE" } else { "PAPER" };
    let adapter = Arc::new(BinanceAdapter::new(api_key, &private_key, environment)?);
    let identity = AccountIdentity {
        account_id: args.account_id,
        venue: "BINANCE".into(),
        environment: environment.into(),
        external_account_ref: args.account_ref.clone(),
    };
    let session_nonce = Uuid::new_v4().to_string();
    let link = V2RuntimeLink::spawn(V2LinkConfig {
        control_url: format!("{}/v2/control?deployment={}&nonce={session_nonce}", args.oms_url.trim_end_matches('/'), args.deployment),
        events_url: format!("{}/v2/events?deployment={}&nonce={session_nonce}", args.oms_url.trim_end_matches('/'), args.deployment),
        token: args.token.clone(),
        ca_pem: args.ca.as_ref().map(std::fs::read).transpose()?,
        account: identity.clone(),
        epoch: args.epoch,
        session_nonce: session_nonce.clone(),
        policy_revision: 0,
    });
    let bootstrap = receive_bootstrap(&link).await?;
    let venue = BinanceRuntimeVenue::new(adapter.clone(), identity.clone())?;
    let runtime = Runtime::start(
        RuntimeConfig {
            identity,
            epoch: args.epoch,
            bootstrap,
            journal_path: args.journal_dir.join("journal"),
            max_journal_bytes: MAX_JOURNAL_BYTES,
            compact_above_bytes: COMPACT_ABOVE_BYTES,
            event_queue_capacity: EVENT_QUEUE,
            control_url: Some(args.oms_url.clone()),
            events_url: Some(args.oms_url.clone()),
            credential_reference: None,
            link: Some(link),
            session_nonce,
        },
        Arc::new(venue),
    )
    .await?;
    let mut mid = spawn_mid_feed(&args.symbol);
    let mut mm = MarketMaker { args, runtime, working: Vec::new() };
    let mut requote = tokio::time::interval(Duration::from_secs(2));
    let mut watch_switch = tokio::time::interval(Duration::from_millis(250));
    let mut events = tokio::time::interval(Duration::from_millis(100));
    let mut was_trading = false;

    loop {
        tokio::select! {
            _ = events.tick() => {
                while mm.runtime.next_event().is_some() {}
            }
            _ = watch_switch.tick() => {
                let trading = mm.may_trade().await;
                if was_trading && !trading {
                    warn!(state = ?mm.runtime.state().await, "runtime stopped: cancelling quotes");
                    mm.cancel_all().await;
                }
                if !was_trading && trading {
                    info!("runtime ready: quoting");
                }
                was_trading = trading;
            }
            _ = requote.tick() => {
                if !was_trading {
                    continue;
                }
                let Some(m) = *mid.borrow_and_update() else {
                    warn!("no mid price yet; not quoting");
                    continue;
                };
                mm.cancel_all().await;
                let (bid, ask) = quote::quotes(m, mm.args.half_spread_bps, mm.args.tick);
                for (buy, price) in [(true, bid), (false, ask)] {
                    if !mm.may_trade().await {
                        break;
                    }
                    mm.quote(buy, price, m).await;
                }
            }
        }
        if mm.runtime.state().await == RuntimeState::Suspended && !mm.working.is_empty() {
            mm.cancel_all().await;
        }
    }
}
