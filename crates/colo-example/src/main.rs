//! Reference co-located market maker. Quotes one bid and one ask around the
//! mid on one Binance pair, every order through the risk library, and only
//! while the control link's dead man's switch allows it.
//!
//! colo-example --link wss://HOST:PORT --token T --ca PEM --symbol BTCUSDT
//! (Binance key from BINANCE_API_KEY / BINANCE_PRIVATE_KEY; testnet unless --live)

mod quote;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use colo_link::client::{DeadMan, Link, LinkConfig};
use colo_link::proto::{FromStrategy, ToStrategy};
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
    #[arg(long)]
    live: bool,
    /// Keep quoting after a kill: to watch the OMS cancel on sight.
    #[arg(long)]
    ignore_kill: bool,
}

/// Mid prices from Binance's public feed.
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

struct OrderIntent<'a> {
    symbol: &'a str,
    quote_currency: &'a str,
    buy: bool,
    qty: f64,
    price: f64,
}

struct Rejection {
    code: String,
    message: String,
}

/// A small local admission wrapper over the shared pure evaluator. The
/// production runtime adds durable reservations; this example demonstrates the
/// same rule source on the v1 control link.
struct RiskBook {
    limits: risk::Limits,
    version: u64,
    positions: BTreeMap<String, f64>,
    state: risk::TradingState,
    working_buy: f64,
    working_sell: f64,
    accepted: VecDeque<Instant>,
}

impl RiskBook {
    fn new(limits: risk::Limits, version: u64) -> Self {
        Self {
            limits,
            version,
            positions: BTreeMap::new(),
            state: risk::TradingState::Active,
            working_buy: 0.0,
            working_sell: 0.0,
            accepted: VecDeque::new(),
        }
    }

    fn set_limits(&mut self, limits: risk::Limits, version: u64) {
        self.limits = limits;
        self.version = version;
    }

    fn set_positions(&mut self, positions: impl IntoIterator<Item = (String, f64)>) {
        self.positions = positions.into_iter().collect();
    }

    fn set_state(&mut self, state: risk::TradingState) {
        self.state = state;
    }

    fn release_all(&mut self) {
        self.working_buy = 0.0;
        self.working_sell = 0.0;
    }

    fn check(&mut self, intent: &OrderIntent<'_>, mid: f64, now: Instant) -> Result<(), Rejection> {
        while self.accepted.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            self.accepted.pop_front();
        }
        if self.limits.max_orders_per_sec.is_some_and(|max| self.accepted.len() >= max as usize) {
            return Err(Rejection { code: "rate_limited".into(), message: "order rate exceeded".into() });
        }
        let side = if intent.buy { risk::Side::Buy } else { risk::Side::Sell };
        let exposure = risk::Exposure {
            position_qty: self.positions.get(intent.symbol).copied().unwrap_or(0.0),
            working_buy_qty: self.working_buy,
            working_sell_qty: self.working_sell,
        };
        risk::evaluate(&risk::RiskInput {
            order: risk::OrderInput { side, quantity: intent.qty, limit_price: Some(intent.price), multiplier: 1.0 },
            exposure,
            limits: self.limits,
            state: self.state,
            valuation: risk::Valuation {
                order_price: Some(mid),
                position_price: Some(mid),
                reference_price: Some(mid),
            },
        })
        .map_err(|r| Rejection { code: r.code, message: r.message })?;
        if !risk::USD_LIKE.contains(&intent.quote_currency) && intent.quote_currency != intent.symbol {
            // The shared evaluator already rejects inherited notional defaults
            // for a non-USD quote; nothing extra is inferred here.
        }
        if intent.buy {
            self.working_buy += intent.qty;
        } else {
            self.working_sell += intent.qty;
        }
        self.accepted.push_back(now);
        Ok(())
    }
}

struct MarketMaker {
    args: Args,
    adapter: Arc<BinanceAdapter>,
    link: Link,
    dead: DeadMan,
    risk: RiskBook,
    working: Vec<String>,
    n: u64,
}

impl MarketMaker {
    fn may_trade(&self) -> bool {
        self.args.ignore_kill || self.dead.may_trade(Instant::now())
    }

    async fn cancel_all(&mut self) {
        for id in self.working.drain(..) {
            if let Err(e) = self.adapter.cancel_order(&id, &self.args.symbol).await {
                warn!(order = %id, error = %e, "cancel failed");
            }
        }
        self.risk.release_all();
    }

    async fn on_control(&mut self, msg: ToStrategy) {
        self.dead.heard(Instant::now());
        self.dead.apply(&msg);
        match msg {
            ToStrategy::State { active, limits, limits_version, positions, dead_man_secs } => {
                info!(active, limits_version, dead_man_secs, "state from the OMS");
                self.risk.set_limits(limits, limits_version);
                self.risk.set_state(if active { risk::TradingState::Active } else { risk::TradingState::Halted });
                self.risk.set_positions(positions.into_iter().map(|p| (p.symbol, p.qty)));
                self.link.send(FromStrategy::AckLimits { limits_version }).await;
            }
            ToStrategy::Limits { limits, limits_version } => {
                self.risk.set_limits(limits, limits_version);
                self.link.send(FromStrategy::AckLimits { limits_version }).await;
            }
            ToStrategy::Positions { positions } => self.risk.set_positions(positions.into_iter().map(|p| (p.symbol, p.qty))),
            ToStrategy::Kill => {
                info!("killed by the OMS");
                self.risk.set_state(risk::TradingState::Halted);
                // Acknowledge first: the OMS is cancelling with its own key anyway.
                self.link.send(FromStrategy::AckKill).await;
                if !self.args.ignore_kill {
                    self.cancel_all().await;
                }
            }
            ToStrategy::Resume => {
                self.risk.set_state(risk::TradingState::Active);
                info!("resumed by the OMS");
            }
            ToStrategy::Heartbeat => {}
        }
    }

    async fn quote(&mut self, buy: bool, price: f64, mid: f64) {
        let intent = OrderIntent { symbol: &self.args.symbol, quote_currency: &self.args.quote_currency, buy, qty: self.args.qty, price };
        if let Err(r) = self.risk.check(&intent, mid, Instant::now()) {
            warn!(code = r.code, message = %r.message, "risk refused a quote");
            return;
        }
        self.n += 1;
        let req = BrokerOrderRequest {
            order_id: quote::client_id(&self.args.tag, self.n),
            symbol: self.args.symbol.clone(),
            native_id: None,
            quantity: self.args.qty,
            side: if buy { "buy" } else { "sell" }.into(),
            order_type: "limit".into(),
            time_in_force: "gtc".into(),
            limit_price: Some(price),
            external_account_ref: String::new(),
        };
        match self.adapter.submit_order(&req).await {
            Ok(resp) => {
                info!(side = if buy { "buy" } else { "sell" }, price, order = %resp.external_order_id, "quote placed");
                self.working.push(resp.external_order_id);
            }
            Err(e) => warn!(error = %e, "quote not placed"),
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

    let mut mm = MarketMaker {
        // The OMS's first state sets the real timeout.
        dead: DeadMan::new(Duration::from_secs(5), Instant::now()),
        risk: RiskBook::new(risk::Limits::default(), 0),
        working: Vec::new(),
        n: 0,
        adapter,
        link,
        args,
    };
    let mut requote = tokio::time::interval(Duration::from_secs(2));
    // The switch is checked far more often than quotes are refreshed, so
    // quotes come down within a fraction of a second of the timeout.
    let mut watch_switch = tokio::time::interval(Duration::from_millis(250));
    let mut was_trading = false;

    loop {
        tokio::select! {
            msg = control.recv() => {
                let Some(msg) = msg else { break };
                mm.on_control(msg).await;
            }
            _ = watch_switch.tick() => {
                let trading = mm.may_trade();
                if was_trading && !trading {
                    warn!("lost the OMS or stopped by it: cancelling quotes");
                    mm.cancel_all().await;
                }
                if !was_trading && trading {
                    info!("cleared to trade: quoting");
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
                    // A kill or a lost OMS that arrived while this cycle
                    // waited on the venue stops the very next order.
                    while let Ok(msg) = control.try_recv() {
                        mm.on_control(msg).await;
                    }
                    if !mm.may_trade() {
                        break;
                    }
                    mm.quote(buy, price, m).await;
                }
            }
        }
    }
    Ok(())
}
