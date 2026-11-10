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
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::StreamExt;
use ibapi::contracts::{Contract, Currency, Exchange, Symbol};
use ibapi::market_data::historical::{BarSize, WhatToShow};
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

#[async_trait]
pub trait TwsClient: Send + Sync {
    async fn next_order_id(&self) -> Result<i32, TwsError>;
    async fn place(&self, o: &TwsOrder) -> Result<(), TwsError>;
    async fn cancel(&self, order_id: i32) -> Result<(), TwsError>;
    /// Orders working at the broker: `(order_id, order_ref, state)`.
    async fn open_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError>;
    /// Orders that finished (API orders only).
    async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError>;
    /// Today's `Execution` and `Commission` events.
    async fn executions_today(&self) -> Result<Vec<TwsEvent>, TwsError>;
    async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError>;
    async fn matching_symbols(&self, pattern: &str) -> Result<Vec<TwsContract>, TwsError>;
    /// Delayed snapshot close, else the last daily bar's close, else `None`.
    async fn prev_close(&self, con_id: i64) -> Result<Option<f64>, TwsError>;
    async fn managed_accounts(&self) -> Result<Vec<String>, TwsError>;
    fn events(&self) -> broadcast::Receiver<TwsEvent>;
    fn connected(&self) -> bool;
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
                        Err(e) => {
                            tracing::warn!(error = %e, "TWS order-update stream error");
                            break;
                        }
                    }
                }
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
                time: parse_exec_time(&e.time),
            }]
        }
        OrderUpdate::CommissionReport(c) => {
            vec![TwsEvent::Commission { exec_id: c.execution_id, amount: c.commission, currency: c.currency }]
        }
        _ => vec![],
    }
}

/// IB sends execution times as `yyyymmdd hh:mm:ss` (UTC on current gateways; older
/// ones append a zone name or use `yyyymmdd-hh:mm:ss`). Unparseable -> now.
fn parse_exec_time(s: &str) -> DateTime<Utc> {
    let mut parts = s.split([' ', '-']).filter(|p| !p.is_empty());
    let (Some(d), Some(t)) = (parts.next(), parts.next()) else { return Utc::now() };
    NaiveDateTime::parse_from_str(&format!("{d} {t}"), "%Y%m%d %H:%M:%S").map_or_else(|_| Utc::now(), |n| n.and_utc())
}

fn stock_contract(con_id: i64, exchange: &str, primary: &str, currency: &str) -> Contract {
    Contract {
        contract_id: con_id as i32,
        exchange: Exchange::new(exchange),
        primary_exchange: Exchange::new(primary),
        currency: Currency::new(currency),
        ..Contract::default()
    }
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

fn to_tws_order_state(d: &OrderData, status: Option<(f64, f64, Option<f64>)>) -> (i32, String, TwsOrderState) {
    let o = &d.order;
    let (filled, remaining, avg) = status.unwrap_or((o.filled_quantity, o.total_quantity, None));
    let order = TwsOrder {
        order_id: d.order_id,
        con_id: d.contract.contract_id as i64,
        exchange: d.contract.exchange.to_string(),
        primary_exchange: d.contract.primary_exchange.to_string(),
        currency: d.contract.currency.to_string(),
        action: if matches!(o.action, Action::Buy) { Side::Buy } else { Side::Sell },
        qty: o.total_quantity + o.filled_quantity,
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
        avg_fill_price: avg.unwrap_or(0.0),
        symbol: d.contract.symbol.to_string(),
        order,
    };
    (d.order_id, o.order_ref.clone(), state)
}

/// Joins an orders stream's `OrderData` rows with their `OrderStatus` rows.
fn join_orders(rows: Vec<Orders>) -> Vec<(i32, String, TwsOrderState)> {
    let mut statuses = HashMap::new();
    for r in &rows {
        if let Orders::OrderStatus(s) = r {
            statuses.insert(s.order_id, (s.filled, s.remaining, s.average_fill_price));
        }
    }
    rows.iter()
        .filter_map(|r| match r {
            Orders::OrderData(d) => Some(to_tws_order_state(d, statuses.get(&d.order_id).copied())),
            _ => None,
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
        let contract = stock_contract(o.con_id, &o.exchange, &o.primary_exchange, &o.currency);
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
        Ok(join_orders(sub.collect_to_end_within(REQUEST_TIMEOUT).await?))
    }

    async fn completed_orders(&self) -> Result<Vec<(i32, String, TwsOrderState)>, TwsError> {
        self.ensure_connected()?;
        let mut sub = self.client.completed_orders(true).await?;
        Ok(join_orders(sub.collect_to_end_within(REQUEST_TIMEOUT).await?))
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
                    time: parse_exec_time(&d.execution.time),
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
            contract_id: q.con_id.unwrap_or(0) as i32,
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
        let contract = stock_contract(con_id, "SMART", "", "");
        if let Ok(ticks) = self.client.market_data(&contract).snapshot_once(REQUEST_TIMEOUT).await {
            if let Some(close) = Quote::from_ticks(ticks.iter().filter(|t| matches!(t, TickTypes::Price(_)))).close {
                return Ok(Some(close));
            }
        }
        let data = self
            .client
            .historical_data(&contract, BarSize::Day)
            .duration(ibapi::market_data::historical::Duration::DAY)
            .what_to_show(WhatToShow::Trades)
            .fetch()
            .await?;
        Ok(data.bars.last().map(|b| b.close))
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
    fn execution_times_parse_both_ib_formats() {
        let want = "2026-11-10T14:30:05Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(parse_exec_time("20261110 14:30:05"), want);
        assert_eq!(parse_exec_time("20261110-14:30:05"), want);
        assert_eq!(parse_exec_time("20261110  14:30:05 America/New_York"), want);
    }
}
