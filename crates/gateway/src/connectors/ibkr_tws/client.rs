//! The seam between the IBKR_TWS connector and the TWS API.
//!
//! [`TwsClient`] is everything the adapter, the instrument resolver and the
//! daily-stats refresh need from IB Gateway. [`IbapiClient`] implements it over
//! the `ibapi` crate's async client; tests use the scripted [`FakeTws`].

use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use futures_util::StreamExt;
use ibapi::contracts::{Contract, Currency, Exchange, Symbol};
use ibapi::market_data::historical::{BarSize, BarTimestamp, WhatToShow};
use ibapi::market_data::realtime::{Quote, TickTypes};
use ibapi::market_data::MarketDataType;
use ibapi::orders::{order_builder, Action, ExecutionFilter, Executions, OrderData, OrderUpdate, Orders, TimeInForce};
use ibapi::subscriptions::SubscriptionItem;
use ibapi::Client;
use tokio::sync::broadcast;

/// The API client id the OMS connects with.
const CLIENT_ID: i32 = 1;
/// How long a one-shot snapshot or list request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_CAPACITY: usize = 1024;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum TwsError {
    #[error("not connected to IB Gateway")]
    NotConnected,
    #[error("IB error {code}: {message}")]
    Api { code: i32, message: String },
    #[error("{0}")]
    Other(String),
}

impl From<ibapi::Error> for TwsError {
    fn from(e: ibapi::Error) -> Self {
        match e {
            ibapi::Error::Notice(n) => TwsError::Api { code: n.code, message: n.message },
            e if e.is_connection_lost() => TwsError::NotConnected,
            e => TwsError::Other(e.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderKind {
    Mkt,
    Lmt(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Day,
    Gtc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TwsContract {
    pub con_id: i64,
    pub symbol: String,
    pub exchange: String,
    pub primary_exchange: String,
    pub currency: String,
    pub min_tick: f64,
    pub lot_size: Option<f64>,
    pub name: String,
    pub isin: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TwsOrder {
    pub order_id: i32,
    pub con_id: i64,
    pub exchange: String,
    pub primary_exchange: String,
    pub currency: String,
    pub action: Side,
    pub qty: f64,
    pub order_type: OrderKind,
    pub tif: Tif,
    pub order_ref: String,
    pub account: String,
}

/// One order as the broker reports it: status, fills, and the order echoed back
/// (enough for a `BrokerOpenOrder`: order ref, order id, symbol, filled qty).
#[derive(Debug, Clone, PartialEq)]
pub struct TwsOrderState {
    /// IB's status string (`Submitted`, `Filled`, `Cancelled`, ...).
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub avg_fill_price: f64,
    pub symbol: String,
    pub order: TwsOrder,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TwsEvent {
    Status { order_id: i32, order_ref: String, status: String, filled: f64, remaining: f64 },
    Execution { exec_id: String, order_id: i32, order_ref: String, qty: f64, price: f64, time: DateTime<Utc> },
    Commission { exec_id: String, amount: f64, currency: String },
    Error { order_id: Option<i32>, code: i32, message: String },
    Disconnected,
}

/// A stock lookup; sec type is always STK, exchange defaults to SMART.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ContractQuery {
    pub symbol: Option<String>,
    pub exchange: Option<String>,
    pub currency: Option<String>,
    pub con_id: Option<i64>,
}

/// One IB Gateway API session. Order listings (`open_orders`,
/// `completed_orders`) and the order-update stream cover only orders placed by
/// this API client id: orders entered by hand in TWS or the IBKR apps are
/// invisible, so recon cannot see or settle them.
#[async_trait]
pub trait TwsClient: Send + Sync {
    async fn next_order_id(&self) -> Result<i32, TwsError>;
    async fn place(&self, o: &TwsOrder) -> Result<(), TwsError>;
    async fn cancel(&self, order_id: i32) -> Result<(), TwsError>;
    /// Orders working at the broker: `(order_id, order_ref, state)`.
    ///
    /// Also teaches the client each order's ref: `Status` events carry the
    /// `order_ref` only for ids it has seen, so callers run this on every
    /// (re)connect before relying on `Status.order_ref`.
    async fn open_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError>;
    /// Orders that finished (API orders only); also records their refs, like `open_orders`.
    async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError>;
    /// Today's `Execution` and `Commission` events. TWS also replays these onto
    /// [`TwsClient::events`] (the order-update stream), so consumers must dedupe by `exec_id`.
    async fn executions_today(&self) -> Result<Vec<TwsEvent>, TwsError>;
    async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError>;
    /// Symbol search. Results carry `min_tick: 0.0`, no lot size and no ISIN:
    /// follow up with `contract_details` for the tradable facts.
    async fn matching_symbols(&self, pattern: &str) -> Result<Vec<TwsContract>, TwsError>;
    /// Delayed snapshot close, else the last daily bar's close, else `None`.
    async fn prev_close(&self, con_id: i64) -> Result<Option<f64>, TwsError>;
    async fn managed_accounts(&self) -> Result<Vec<String>, TwsError>;
    fn events(&self) -> broadcast::Receiver<TwsEvent>;
    fn connected(&self) -> bool;
    /// Closes the API socket so the client id is free for the next session.
    /// Synchronous so a drop guard can call it.
    fn shutdown(&self);
}

// ---------------------------------------------------------------------------
// ibapi implementation
// ---------------------------------------------------------------------------

type OrderRefs = Arc<Mutex<HashMap<i32, String>>>;

/// [`TwsClient`] over `ibapi`'s async client. One order-update subscription,
/// owned by a background task, feeds [`TwsClient::events`].
pub struct IbapiClient {
    client: Arc<Client>,
    events: broadcast::Sender<TwsEvent>,
    order_refs: OrderRefs,
    pump: tokio::task::JoinHandle<()>,
}

impl IbapiClient {
    /// Connects to IB Gateway on `127.0.0.1:<port>` as client id 1 and asks for
    /// delayed (type 3) market data.
    pub async fn connect(port: u16) -> Result<Self, TwsError> {
        let client = Client::connect(&format!("127.0.0.1:{port}"), CLIENT_ID).await?;
        client.switch_market_data_type(MarketDataType::Delayed).await?;
        let client = Arc::new(client);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let order_refs: OrderRefs = Arc::default();
        let pump = tokio::spawn(pump_order_updates(client.clone(), events.clone(), order_refs.clone()));
        Ok(Self { client, events, order_refs, pump })
    }

    /// Today's date on the instrument's exchange (from its contract details'
    /// timezone id); UTC's date, with a warning, if that can't be determined.
    async fn exchange_today(&self, contract: &Contract) -> NaiveDate {
        let zone = match self.client.contract_details(contract).await {
            Ok(d) => d.first().and_then(|d| d.time_zone_id.parse::<chrono_tz::Tz>().ok()),
            Err(_) => None,
        };
        match zone {
            Some(tz) => Utc::now().with_timezone(&tz).date_naive(),
            None => {
                tracing::warn!(con_id = contract.contract_id, "exchange timezone unknown; using the UTC date for prev_close");
                Utc::now().date_naive()
            }
        }
    }

    fn ensure_connected(&self) -> Result<(), TwsError> {
        self.connected().then_some(()).ok_or(TwsError::NotConnected)
    }
}

impl Drop for IbapiClient {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// Reads the single order-update subscription for the client's life,
/// resubscribing after a connection loss and announcing it with `Disconnected`.
async fn pump_order_updates(client: Arc<Client>, tx: broadcast::Sender<TwsEvent>, refs: OrderRefs) {
    loop {
        match client.order_update_stream().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(SubscriptionItem::Data(update)) => {
                            for ev in update_events(update, &refs) {
                                let _ = tx.send(ev);
                            }
                        }
                        Ok(SubscriptionItem::Notice(n)) => {
                            let _ = tx.send(TwsEvent::Error { order_id: n.request_id, code: n.code, message: n.message });
                        }
                        Err(ibapi::Error::Notice(n)) => {
                            let _ = tx.send(TwsEvent::Error { order_id: n.request_id, code: n.code, message: n.message });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "TWS order-update stream error");
                            break;
                        }
                    }
                }
            }
            // Still connected but the subscription is taken: back off, don't announce a disconnect.
            Err(ibapi::Error::AlreadySubscribed) if client.is_connected() => {
                tracing::warn!("TWS order-update stream is already subscribed; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Err(e) => tracing::warn!(error = %e, "TWS order-update subscribe failed"),
        }
        let _ = tx.send(TwsEvent::Disconnected);
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Terminal (reconnect gave up / shut down): stop. A timeout just retries.
        if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(60), client.wait_connected()).await {
            tracing::warn!(error = %e, "TWS client is gone; order-update pump stops");
            return;
        }
    }
}

fn update_events(update: OrderUpdate, refs: &OrderRefs) -> Vec<TwsEvent> {
    let lookup = |id: i32| refs.lock().unwrap().get(&id).cloned().unwrap_or_default();
    match update {
        OrderUpdate::OrderStatus(s) => vec![TwsEvent::Status {
            order_id: s.order_id,
            order_ref: lookup(s.order_id),
            status: s.status.as_str().to_string(),
            filled: s.filled,
            remaining: s.remaining,
        }],
        OrderUpdate::OpenOrder(d) => {
            if !d.order.order_ref.is_empty() {
                refs.lock().unwrap().insert(d.order_id, d.order.order_ref.clone());
            }
            vec![]
        }
        OrderUpdate::ExecutionData(d) => {
            let e = d.execution;
            let order_ref = if e.order_reference.is_empty() { lookup(e.order_id) } else { e.order_reference };
            vec![TwsEvent::Execution {
                exec_id: e.execution_id,
                order_id: e.order_id,
                order_ref,
                qty: e.shares,
                price: e.price,
                time: exec_time_or_now(&e.time),
            }]
        }
        OrderUpdate::CommissionReport(c) => {
            vec![TwsEvent::Commission { exec_id: c.execution_id, amount: c.commission, currency: c.currency }]
        }
        _ => vec![],
    }
}

/// TWS sends execution times as `yyyymmdd  hh:mm:ss <IANA zone>` in the login
/// timezone (`ibapi` passes the string through raw); a zone-less `yyyymmdd hh:mm:ss`
/// or `yyyymmdd-hh:mm:ss` is taken as UTC. `None` when unparseable or the zone is unknown.
fn parse_exec_time(s: &str) -> Option<DateTime<Utc>> {
    let mut parts = s.split([' ', '-']).filter(|p| !p.is_empty());
    let (d, t) = (parts.next()?, parts.next()?);
    let naive = NaiveDateTime::parse_from_str(&format!("{d} {t}"), "%Y%m%d %H:%M:%S").ok()?;
    match parts.next() {
        None => Some(naive.and_utc()),
        Some(zone) => {
            let tz: chrono_tz::Tz = zone.parse().ok()?;
            Some(tz.from_local_datetime(&naive).earliest()?.with_timezone(&Utc))
        }
    }
}

/// [`parse_exec_time`], falling back to now (with a warning) so a bad timestamp
/// never drops a fill. The fill's identity is its exec id, not its time.
fn exec_time_or_now(s: &str) -> DateTime<Utc> {
    parse_exec_time(s).unwrap_or_else(|| {
        tracing::warn!(time = s, "unparseable TWS execution time; using receive time");
        Utc::now()
    })
}

/// The last daily bar strictly before `today` (the exchange's date): the bar
/// for today, if present, is a partial session and not a previous close.
fn pick_prev_close(bars: &[(NaiveDate, f64)], today: NaiveDate) -> Option<f64> {
    bars.iter().filter(|(d, _)| *d < today).max_by_key(|(d, _)| *d).map(|(_, c)| *c)
}

fn bar_date(ts: &BarTimestamp) -> NaiveDate {
    let d = match ts {
        BarTimestamp::Date(d) => *d,
        BarTimestamp::DateTime(t) => t.date(),
    };
    NaiveDate::from_ymd_opt(d.year(), d.month() as u32, d.day() as u32).unwrap_or_default()
}

fn con_id_i32(con_id: i64) -> Result<i32, TwsError> {
    i32::try_from(con_id).map_err(|_| TwsError::Other(format!("conId {con_id} does not fit the TWS API's 32-bit contract id")))
}

fn stock_contract(con_id: i64, exchange: &str, primary: &str, currency: &str) -> Result<Contract, TwsError> {
    Ok(Contract {
        contract_id: con_id_i32(con_id)?,
        exchange: Exchange::new(exchange),
        primary_exchange: Exchange::new(primary),
        currency: Currency::new(currency),
        ..Contract::default()
    })
}

fn to_tws_contract(c: &Contract, min_tick: f64, lot_size: Option<f64>, name: String, isin: Option<String>) -> TwsContract {
    TwsContract {
        con_id: c.contract_id as i64,
        symbol: c.symbol.to_string(),
        exchange: c.exchange.to_string(),
        primary_exchange: c.primary_exchange.to_string(),
        currency: c.currency.to_string(),
        min_tick,
        lot_size,
        name,
        isin,
    }
}

/// `(ordered qty, filled, remaining, avg fill price)` of one order report.
/// Open-order reports carry the full size in `total_quantity`; completed-order
/// reports carry the unfilled remainder there (ordered = total + filled).
/// An `OrderStatus` row, when present, is authoritative for fills.
fn order_quantities(completed: bool, total: f64, filled_qty: f64, status: Option<(f64, f64, Option<f64>)>) -> (f64, f64, f64, f64) {
    let (qty, filled, remaining) = if completed {
        (total + filled_qty, filled_qty, total)
    } else {
        (total, filled_qty, (total - filled_qty).max(0.0))
    };
    match status {
        Some((f, r, avg)) => (qty, f, r, avg.unwrap_or(0.0)),
        None => (qty, filled, remaining, 0.0),
    }
}

fn to_tws_order_state(d: &OrderData, completed: bool, status: Option<(f64, f64, Option<f64>)>) -> (i32, String, TwsOrderState) {
    let o = &d.order;
    let (qty, filled, remaining, avg_fill_price) = order_quantities(completed, o.total_quantity, o.filled_quantity, status);
    let order = TwsOrder {
        order_id: d.order_id,
        con_id: d.contract.contract_id as i64,
        exchange: d.contract.exchange.to_string(),
        primary_exchange: d.contract.primary_exchange.to_string(),
        currency: d.contract.currency.to_string(),
        action: if matches!(o.action, Action::Buy) { Side::Buy } else { Side::Sell },
        qty,
        order_type: match (o.order_type.as_str(), o.limit_price) {
            ("LMT", Some(p)) => OrderKind::Lmt(p),
            _ => OrderKind::Mkt,
        },
        tif: if matches!(o.tif, TimeInForce::GoodTillCanceled) { Tif::Gtc } else { Tif::Day },
        order_ref: o.order_ref.clone(),
        account: o.account.clone(),
    };
    let state = TwsOrderState {
        status: d.order_state.status.as_str().to_string(),
        filled,
        remaining,
        avg_fill_price,
        symbol: d.contract.symbol.to_string(),
        order,
    };
    (d.order_id, o.order_ref.clone(), state)
}

/// Joins an orders stream's `OrderData` rows with their `OrderStatus` rows.
fn join_orders(rows: Vec<Orders>, completed: bool, refs: &OrderRefs) -> Vec<(i32, String, TwsOrderState)> {
    let mut statuses = HashMap::new();
    for r in &rows {
        if let Orders::OrderStatus(s) = r {
            statuses.insert(s.order_id, (s.filled, s.remaining, s.average_fill_price));
        }
    }
    rows.iter()
        .filter_map(|r| match r {
            Orders::OrderData(d) => Some(to_tws_order_state(d, completed, statuses.get(&d.order_id).copied())),
            _ => None,
        })
        .inspect(|(id, order_ref, _)| {
            if !order_ref.is_empty() {
                refs.lock().unwrap().insert(*id, order_ref.clone());
            }
        })
        .collect()
}

#[async_trait]
impl TwsClient for IbapiClient {
    async fn next_order_id(&self) -> Result<i32, TwsError> {
        self.ensure_connected()?;
        Ok(self.client.next_order_id())
    }

    async fn place(&self, o: &TwsOrder) -> Result<(), TwsError> {
        self.ensure_connected()?;
        let action = match o.action {
            Side::Buy => Action::Buy,
            Side::Sell => Action::Sell,
        };
        let mut order = match o.order_type {
            OrderKind::Mkt => order_builder::market_order(action, o.qty),
            OrderKind::Lmt(px) => order_builder::limit_order(action, o.qty, px),
        };
        order.tif = match o.tif {
            Tif::Day => TimeInForce::Day,
            Tif::Gtc => TimeInForce::GoodTillCanceled,
        };
        order.order_ref = o.order_ref.clone();
        order.account = o.account.clone();
        let contract = stock_contract(o.con_id, &o.exchange, &o.primary_exchange, &o.currency)?;
        self.order_refs.lock().unwrap().insert(o.order_id, o.order_ref.clone());
        self.client.submit_order(o.order_id, &contract, &order).await?;
        Ok(())
    }

    async fn cancel(&self, order_id: i32) -> Result<(), TwsError> {
        self.ensure_connected()?;
        // The confirmation arrives on the order-update stream; drop this subscription.
        drop(self.client.cancel_order(order_id, "").await?);
        Ok(())
    }

    async fn open_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError> {
        self.ensure_connected()?;
        let mut sub = self.client.open_orders().await?;
        Ok(join_orders(sub.collect_to_end_within(REQUEST_TIMEOUT).await?, false, &self.order_refs))
    }

    async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError> {
        self.ensure_connected()?;
        let mut sub = self.client.completed_orders(true).await?;
        Ok(join_orders(sub.collect_to_end_within(REQUEST_TIMEOUT).await?, true, &self.order_refs))
    }

    async fn executions_today(&self) -> Result<Vec<TwsEvent>, TwsError> {
        self.ensure_connected()?;
        let mut sub = self.client.executions(ExecutionFilter::default()).await?;
        let rows = sub.collect_to_end_within(REQUEST_TIMEOUT).await?;
        Ok(rows
            .into_iter()
            .map(|r| match r {
                Executions::ExecutionData(d) => TwsEvent::Execution {
                    exec_id: d.execution.execution_id,
                    order_id: d.execution.order_id,
                    order_ref: d.execution.order_reference,
                    qty: d.execution.shares,
                    price: d.execution.price,
                    time: exec_time_or_now(&d.execution.time),
                },
                Executions::CommissionReport(c) => {
                    TwsEvent::Commission { exec_id: c.execution_id, amount: c.commission, currency: c.currency }
                }
            })
            .collect())
    }

    async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError> {
        self.ensure_connected()?;
        let contract = Contract {
            contract_id: con_id_i32(q.con_id.unwrap_or(0))?,
            symbol: Symbol::new(q.symbol.clone().unwrap_or_default()),
            exchange: Exchange::new(q.exchange.as_deref().unwrap_or("SMART")),
            currency: Currency::new(q.currency.clone().unwrap_or_default()),
            primary_exchange: Exchange::new(""),
            ..Contract::default()
        };
        let details = self.client.contract_details(&contract).await?;
        Ok(details
            .into_iter()
            .map(|d| {
                let isin = d.sec_id_list.iter().find(|t| t.tag == "ISIN").map(|t| t.value.clone());
                to_tws_contract(&d.contract, d.min_tick, d.size_increment, d.long_name, isin)
            })
            .collect())
    }

    async fn matching_symbols(&self, pattern: &str) -> Result<Vec<TwsContract>, TwsError> {
        self.ensure_connected()?;
        let found = self.client.matching_symbols(pattern).await?;
        Ok(found
            .into_iter()
            .map(|d| to_tws_contract(&d.contract, 0.0, None, d.contract.description.clone(), None))
            .collect())
    }

    async fn prev_close(&self, con_id: i64) -> Result<Option<f64>, TwsError> {
        self.ensure_connected()?;
        let contract = stock_contract(con_id, "SMART", "", "")?;
        if let Ok(ticks) = self.client.market_data(&contract).snapshot_once(REQUEST_TIMEOUT).await {
            if let Some(close) = Quote::from_ticks(ticks.iter().filter(|t| matches!(t, TickTypes::Price(_)))).close {
                return Ok(Some(close));
            }
        }
        // Two daily bars: the newest may be today's partial session.
        let data = self
            .client
            .historical_data(&contract, BarSize::Day)
            .duration(ibapi::market_data::historical::Duration::days(2))
            .what_to_show(WhatToShow::Trades)
            .fetch()
            .await?;
        let bars: Vec<(NaiveDate, f64)> = data.bars.iter().map(|b| (bar_date(&b.date), b.close)).collect();
        Ok(pick_prev_close(&bars, self.exchange_today(&contract).await))
    }

    async fn managed_accounts(&self) -> Result<Vec<String>, TwsError> {
        self.ensure_connected()?;
        Ok(self.client.managed_accounts().await?)
    }

    fn events(&self) -> broadcast::Receiver<TwsEvent> {
        self.events.subscribe()
    }

    fn connected(&self) -> bool {
        self.client.is_connected()
    }

    fn shutdown(&self) {
        self.pump.abort();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            rt.spawn(async move { client.disconnect().await });
        }
    }
}

// ---------------------------------------------------------------------------
// Scripted fake for tests
// ---------------------------------------------------------------------------

/// A scripted [`TwsClient`]: contract lookups answer from `details`, orders are
/// recorded, ids count up from 100, events are injected through `events_tx`.
#[cfg(test)]
pub(crate) struct FakeTws {
    pub connected: AtomicBool,
    next_id: AtomicI32,
    pub details: Mutex<HashMap<ContractQuery, Vec<TwsContract>>>,
    pub symbols: Mutex<HashMap<String, Vec<TwsContract>>>,
    pub prev_closes: Mutex<HashMap<i64, f64>>,
    pub accounts: Mutex<Vec<String>>,
    pub open: Mutex<Vec<(i32, String, TwsOrderState)>>,
    pub completed: Mutex<Vec<(i32, String, TwsOrderState)>>,
    pub executions: Mutex<Vec<TwsEvent>>,
    pub placed: Mutex<Vec<TwsOrder>>,
    pub cancelled: Mutex<Vec<i32>>,
    pub events_tx: broadcast::Sender<TwsEvent>,
}

#[cfg(test)]
impl FakeTws {
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(true),
            next_id: AtomicI32::new(100),
            details: Mutex::default(),
            symbols: Mutex::default(),
            prev_closes: Mutex::default(),
            accounts: Mutex::default(),
            open: Mutex::default(),
            completed: Mutex::default(),
            executions: Mutex::default(),
            placed: Mutex::default(),
            cancelled: Mutex::default(),
            events_tx: broadcast::channel(EVENT_CAPACITY).0,
        }
    }

    pub fn set_connected(&self, up: bool) {
        self.connected.store(up, Ordering::SeqCst);
    }

    fn up(&self) -> Result<(), TwsError> {
        self.connected().then_some(()).ok_or(TwsError::NotConnected)
    }
}

#[cfg(test)]
#[async_trait]
impl TwsClient for FakeTws {
    async fn next_order_id(&self) -> Result<i32, TwsError> {
        self.up()?;
        Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    async fn place(&self, o: &TwsOrder) -> Result<(), TwsError> {
        self.up()?;
        self.placed.lock().unwrap().push(o.clone());
        Ok(())
    }

    async fn cancel(&self, order_id: i32) -> Result<(), TwsError> {
        self.up()?;
        self.cancelled.lock().unwrap().push(order_id);
        Ok(())
    }

    async fn open_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError> {
        self.up()?;
        Ok(self.open.lock().unwrap().clone())
    }

    async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError> {
        self.up()?;
        Ok(self.completed.lock().unwrap().clone())
    }

    async fn executions_today(&self) -> Result<Vec<TwsEvent>, TwsError> {
        self.up()?;
        Ok(self.executions.lock().unwrap().clone())
    }

    async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError> {
        self.up()?;
        Ok(self.details.lock().unwrap().get(q).cloned().unwrap_or_default())
    }

    async fn matching_symbols(&self, pattern: &str) -> Result<Vec<TwsContract>, TwsError> {
        self.up()?;
        Ok(self.symbols.lock().unwrap().get(pattern).cloned().unwrap_or_default())
    }

    async fn prev_close(&self, con_id: i64) -> Result<Option<f64>, TwsError> {
        self.up()?;
        Ok(self.prev_closes.lock().unwrap().get(&con_id).copied())
    }

    async fn managed_accounts(&self) -> Result<Vec<String>, TwsError> {
        self.up()?;
        Ok(self.accounts.lock().unwrap().clone())
    }

    fn events(&self) -> broadcast::Receiver<TwsEvent> {
        self.events_tx.subscribe()
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn shutdown(&self) {
        self.set_connected(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_tws_hands_out_increasing_ids() {
        let fake = FakeTws::new();
        assert_eq!(fake.next_order_id().await.unwrap(), 100);
        assert_eq!(fake.next_order_id().await.unwrap(), 101);
    }

    #[tokio::test]
    async fn fake_tws_refuses_when_disconnected_and_records_orders() {
        let fake = FakeTws::new();
        fake.set_connected(false);
        assert_eq!(fake.cancel(7).await, Err(TwsError::NotConnected));
        fake.set_connected(true);
        fake.cancel(7).await.unwrap();
        assert_eq!(*fake.cancelled.lock().unwrap(), vec![7]);
    }

    #[test]
    fn execution_times_convert_their_zone_to_utc() {
        let utc = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        // November: New York is on EST (UTC-5).
        assert_eq!(parse_exec_time("20261110  14:30:05 America/New_York"), Some(utc("2026-11-10T19:30:05Z")));
        // July: EDT (UTC-4).
        assert_eq!(parse_exec_time("20260710  14:30:05 America/New_York"), Some(utc("2026-07-10T18:30:05Z")));
        // Zone-less forms are UTC.
        assert_eq!(parse_exec_time("20261110 14:30:05"), Some(utc("2026-11-10T14:30:05Z")));
        assert_eq!(parse_exec_time("20261110-14:30:05"), Some(utc("2026-11-10T14:30:05Z")));
        // Garbage and unknown zones are None (the caller warns and falls back).
        assert_eq!(parse_exec_time("soon"), None);
        assert_eq!(parse_exec_time("20261110  14:30:05 Mars/Olympus"), None);
    }

    #[test]
    fn prev_close_skips_todays_partial_bar() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        let bars = [(d(11, 9), 100.0), (d(11, 10), 101.5)];
        assert_eq!(pick_prev_close(&bars, d(11, 10)), Some(100.0));
        // Market not yet open today: the newest bar is yesterday's.
        assert_eq!(pick_prev_close(&bars, d(11, 11)), Some(101.5));
        assert_eq!(pick_prev_close(&bars[1..], d(11, 10)), None);
        assert_eq!(pick_prev_close(&[], d(11, 10)), None);
    }

    #[test]
    fn open_and_completed_reports_read_quantities_differently() {
        // Open: total_quantity is the full size; 4 of 10 filled.
        assert_eq!(order_quantities(false, 10.0, 4.0, None), (10.0, 4.0, 6.0, 0.0));
        // Completed: total_quantity is what was left unfilled; ordered = total + filled.
        assert_eq!(order_quantities(true, 0.0, 10.0, None), (10.0, 10.0, 0.0, 0.0));
        assert_eq!(order_quantities(true, 6.0, 4.0, None), (10.0, 4.0, 6.0, 0.0));
        // An OrderStatus row wins for fills and price.
        assert_eq!(order_quantities(false, 10.0, 0.0, Some((4.0, 6.0, Some(9.5)))), (10.0, 4.0, 6.0, 9.5));
    }

    #[test]
    fn con_ids_beyond_i32_are_refused() {
        assert_eq!(con_id_i32(265598), Ok(265598));
        assert!(matches!(con_id_i32(i64::from(i32::MAX) + 1), Err(TwsError::Other(_))));
    }
}
