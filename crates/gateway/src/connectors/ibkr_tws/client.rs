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
use ibapi::contracts::{Contract, Currency, Exchange, SecurityType, Symbol};
use ibapi::market_data::historical::{BarSize, BarTimestamp, WhatToShow};
use ibapi::market_data::realtime::{Quote, TickTypes};
use ibapi::market_data::MarketDataType;
use ibapi::orders::{order_builder, Action, ExecutionFilter, Executions, OrderData, OrderUpdate, Orders, TimeInForce};
use ibapi::subscriptions::SubscriptionItem;
use ibapi::Client;
use tokio::sync::broadcast;

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

/// The IBKR security types the OMS trades. `Bag` is a multi-leg combo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SecType {
    #[default]
    Stk,
    Fut,
    Opt,
    Bag,
}

impl SecType {
    pub fn as_ibkr(self) -> &'static str {
        match self {
            SecType::Stk => "STK",
            SecType::Fut => "FUT",
            SecType::Opt => "OPT",
            SecType::Bag => "BAG",
        }
    }

    pub fn parse(s: &str) -> Option<SecType> {
        match s {
            "STK" => Some(SecType::Stk),
            "FUT" => Some(SecType::Fut),
            "OPT" => Some(SecType::Opt),
            "BAG" => Some(SecType::Bag),
            _ => None,
        }
    }

    fn to_ibapi(self) -> SecurityType {
        match self {
            SecType::Stk => SecurityType::Stock,
            SecType::Fut => SecurityType::Future,
            SecType::Opt => SecurityType::Option,
            SecType::Bag => SecurityType::Spread,
        }
    }
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
    pub sec_type: SecType,
    /// IBKR `localSymbol` (`ESZ6`): the OMS symbol of a derivative. `symbol` is the root (`ES`).
    pub local_symbol: String,
    /// Contract multiplier; 1.0 for stocks.
    pub multiplier: f64,
    /// Last trading day, when the contract has one.
    pub last_trade: Option<NaiveDate>,
    /// Last trading instant (day + time in the contract's zone), when known.
    pub last_trade_at: Option<DateTime<Utc>>,
    pub strike: Option<f64>,
    /// `"C"` or `"P"` for options.
    pub right: Option<String>,
}

/// One leg of a combo order.
#[derive(Debug, Clone, PartialEq)]
pub struct TwsLeg {
    pub con_id: i64,
    pub ratio: i32,
    pub buy: bool,
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
    pub sec_type: SecType,
    /// Empty for single-instrument orders.
    pub legs: Vec<TwsLeg>,
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
    /// IBKR's permanent order id: stable across API client ids and sessions; 0 when unknown.
    pub perm_id: i64,
    pub order: TwsOrder,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TwsEvent {
    /// An order's current state: from `openOrder`, or an `orderStatus` merged
    /// into the last `openOrder` seen for the id.
    Order { order_id: i32, order_ref: String, state: TwsOrderState },
    Execution(TwsExecution),
    Commission { exec_id: String, amount: f64, currency: String },
    Error { order_id: Option<i32>, code: i32, message: String },
    Disconnected,
}

/// One execution as IBKR reports it (`execDetails` / `reqExecutions`).
/// `account` is the IBKR account number it booked to.
#[derive(Debug, Clone, PartialEq)]
pub struct TwsExecution {
    pub exec_id: String,
    pub order_id: i32,
    pub perm_id: i64,
    pub order_ref: String,
    pub account: String,
    pub symbol: String,
    pub buy: bool,
    pub qty: f64,
    pub price: f64,
    pub time: DateTime<Utc>,
}

/// A contract lookup; sec type defaults to STK, exchange to SMART.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ContractQuery {
    pub symbol: Option<String>,
    pub exchange: Option<String>,
    pub currency: Option<String>,
    pub con_id: Option<i64>,
    pub sec_type: SecType,
    pub include_expired: bool,
}

/// One IB Gateway API session. Connected as the master client id 0, the
/// order-update stream and `open_orders` also carry orders entered by hand in
/// TWS or the IBKR apps (`reqAutoOpenOrders(true)`); under any other client id
/// they cover only that client's orders.
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
    /// `Execution` and `Commission` events of `account` since `since` (IBKR
    /// serves about the last 7 days, in one response, no paging). TWS also
    /// replays these onto [`TwsClient::events`], so consumers dedupe by `exec_id`.
    async fn executions(&self, account: &str, since: DateTime<Utc>) -> Result<Vec<TwsEvent>, TwsError>;
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
/// Last `openOrder` per IBKR order id, so an `orderStatus` (which names only
/// the id) becomes a whole order state.
type OrderCache = Arc<Mutex<HashMap<i32, TwsOrderState>>>;

/// [`TwsClient`] over `ibapi`'s async client. One order-update subscription,
/// owned by a background task, feeds [`TwsClient::events`].
pub struct IbapiClient {
    client: Arc<Client>,
    events: broadcast::Sender<TwsEvent>,
    order_refs: OrderRefs,
    pump: tokio::task::JoinHandle<()>,
}

impl IbapiClient {
    /// Connects to IB Gateway on `127.0.0.1:<port>` as `client_id` and asks for
    /// delayed (type 3) market data. As the master client id 0 it also binds
    /// orders entered in TWS (`reqAutoOpenOrders(true)`), so they stream too.
    pub async fn connect(port: u16, client_id: i32) -> Result<Self, TwsError> {
        let client = Client::connect(&format!("127.0.0.1:{port}"), client_id).await?;
        client.switch_market_data_type(MarketDataType::Delayed).await?;
        let client = Arc::new(client);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let order_refs: OrderRefs = Arc::default();
        let pump = tokio::spawn(pump_order_updates(client.clone(), events.clone(), order_refs.clone(), OrderCache::default()));
        if client_id == 0 {
            // Its updates fan out to the order-update stream the pump reads.
            drop(client.auto_open_orders(true).await?);
        }
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
async fn pump_order_updates(client: Arc<Client>, tx: broadcast::Sender<TwsEvent>, refs: OrderRefs, orders: OrderCache) {
    loop {
        match client.order_update_stream().await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(SubscriptionItem::Data(update)) => {
                            for ev in update_events(update, &refs, &orders) {
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

fn update_events(update: OrderUpdate, refs: &OrderRefs, orders: &OrderCache) -> Vec<TwsEvent> {
    let lookup = |id: i32| refs.lock().unwrap().get(&id).cloned().unwrap_or_default();
    match update {
        OrderUpdate::OrderStatus(s) => {
            let mut cache = orders.lock().unwrap();
            let Some(state) = cache.get_mut(&s.order_id) else {
                tracing::warn!(order_id = s.order_id, perm_id = s.perm_id, status = s.status.as_str(), "IBKR orderStatus before its openOrder; skipped");
                return vec![];
            };
            state.status = s.status.as_str().to_string();
            state.filled = s.filled;
            state.remaining = s.remaining;
            state.avg_fill_price = s.average_fill_price.unwrap_or(state.avg_fill_price);
            if s.perm_id != 0 {
                state.perm_id = s.perm_id;
            }
            vec![TwsEvent::Order { order_id: s.order_id, order_ref: lookup(s.order_id), state: state.clone() }]
        }
        OrderUpdate::OpenOrder(d) => {
            if !d.order.order_ref.is_empty() {
                refs.lock().unwrap().insert(d.order_id, d.order.order_ref.clone());
            }
            let (id, order_ref, state) = to_tws_order_state(&d, false, None);
            orders.lock().unwrap().insert(id, state.clone());
            vec![TwsEvent::Order { order_id: id, order_ref, state }]
        }
        OrderUpdate::ExecutionData(d) => {
            let mut e = to_tws_execution(d);
            if e.order_ref.is_empty() {
                e.order_ref = lookup(e.order_id);
            }
            vec![TwsEvent::Execution(e)]
        }
        OrderUpdate::CommissionReport(c) => {
            vec![TwsEvent::Commission { exec_id: c.execution_id, amount: c.commission, currency: c.currency }]
        }
        _ => vec![],
    }
}

fn to_tws_execution(d: ibapi::orders::ExecutionData) -> TwsExecution {
    let e = d.execution;
    TwsExecution {
        time: exec_time_or_now(&e.time),
        exec_id: e.execution_id,
        order_id: e.order_id,
        perm_id: e.perm_id,
        order_ref: e.order_reference,
        account: e.account_number,
        symbol: d.contract.symbol.to_string(),
        buy: matches!(e.side, ibapi::orders::ExecutionSide::Bought),
        qty: e.shares,
        price: e.price,
    }
}

/// `reqExecutions`' time filter: `yyyymmdd-hh:mm:ss`, which TWS reads as UTC.
pub(crate) fn exec_filter_time(t: DateTime<Utc>) -> String {
    t.format("%Y%m%d-%H:%M:%S").to_string()
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

/// A contract by conId. STK routes via the given exchange (SMART) with a
/// primary; FUT/OPT carry the exchange itself (CME), never SMART.
fn ib_contract(con_id: i64, sec_type: SecType, exchange: &str, primary: &str, currency: &str) -> Result<Contract, TwsError> {
    Ok(Contract {
        contract_id: con_id_i32(con_id)?,
        security_type: sec_type.to_ibapi(),
        exchange: Exchange::new(exchange),
        primary_exchange: Exchange::new(primary),
        currency: Currency::new(currency),
        ..Contract::default()
    })
}

/// A contract's last trade day + time in its own zone as a UTC instant.
/// `date` is `YYYYMMDD` (a bare `YYYYMM` month is `None`); `time` is `HH:MM`
/// or `HH:MM:SS`. No time -> `None`.
pub(crate) fn last_trade_instant(date: &str, time: Option<&str>, tz: &str) -> Option<DateTime<Utc>> {
    let time = time.map(str::trim).filter(|t| !t.is_empty())?;
    let d = NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let t = chrono::NaiveTime::parse_from_str(time, "%H:%M:%S").or_else(|_| chrono::NaiveTime::parse_from_str(time, "%H:%M")).ok()?;
    let zone: chrono_tz::Tz = tz.parse().ok()?;
    Some(zone.from_local_datetime(&d.and_time(t)).earliest()?.with_timezone(&Utc))
}

fn to_tws_contract(c: &Contract, min_tick: f64, lot_size: Option<f64>, name: String, isin: Option<String>) -> TwsContract {
    let sec_type = SecType::parse(c.security_type.as_str()).unwrap_or_default();
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
        sec_type,
        local_symbol: c.local_symbol.clone(),
        multiplier: c.multiplier.parse().unwrap_or(1.0),
        last_trade: NaiveDate::parse_from_str(&c.last_trade_date_or_contract_month, "%Y%m%d").ok(),
        last_trade_at: None,
        strike: (c.strike != 0.0).then_some(c.strike),
        right: c.right.as_ref().map(|r| r.as_str().to_string()),
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
        sec_type: SecType::parse(d.contract.security_type.as_str()).unwrap_or_default(),
        legs: vec![],
    };
    let state = TwsOrderState {
        status: d.order_state.status.as_str().to_string(),
        filled,
        remaining,
        avg_fill_price,
        symbol: d.contract.symbol.to_string(),
        perm_id: o.perm_id,
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
        if !o.legs.is_empty() {
            return Err(TwsError::Other("multi-leg orders are not wired yet".into()));
        }
        let contract = ib_contract(o.con_id, o.sec_type, &o.exchange, &o.primary_exchange, &o.currency)?;
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

    async fn executions(&self, account: &str, since: DateTime<Utc>) -> Result<Vec<TwsEvent>, TwsError> {
        self.ensure_connected()?;
        // Days back to reach `since` (today counts as one), at most IBKR's 7.
        let days = (Utc::now().date_naive() - since.date_naive()).num_days().clamp(0, 6) as i32 + 1;
        let filter = ExecutionFilter {
            account_code: account.to_string(),
            time: exec_filter_time(since),
            last_n_days: days,
            ..ExecutionFilter::default()
        };
        let mut sub = self.client.executions(filter).await?;
        let rows = sub.collect_to_end_within(REQUEST_TIMEOUT).await?;
        Ok(rows
            .into_iter()
            .map(|r| match r {
                Executions::ExecutionData(d) => TwsEvent::Execution(to_tws_execution(d)),
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
            security_type: q.sec_type.to_ibapi(),
            include_expired: q.include_expired,
            ..Contract::default()
        };
        let details = self.client.contract_details(&contract).await?;
        Ok(details
            .into_iter()
            .map(|d| {
                let isin = d.sec_id_list.iter().find(|t| t.tag == "ISIN").map(|t| t.value.clone());
                let mut c = to_tws_contract(&d.contract, d.min_tick, d.size_increment, d.long_name.clone(), isin);
                let time = d.last_trade_time.split_whitespace().next();
                c.last_trade_at = last_trade_instant(&d.contract.last_trade_date_or_contract_month, time, &d.time_zone_id);
                c
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
        let contract = ib_contract(con_id, SecType::Stk, "SMART", "", "")?;
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
    /// Contracts registered with `add_contract`; answer any query they match.
    pub contracts: Mutex<Vec<TwsContract>>,
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
            contracts: Mutex::default(),
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

    pub fn add_contract(&self, c: TwsContract) {
        self.contracts.lock().unwrap().push(c);
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

    /// Every scripted event, the executions narrowed to `account` (dropping
    /// those before `since` is the source's job, so it is left to it).
    async fn executions(&self, account: &str, _since: DateTime<Utc>) -> Result<Vec<TwsEvent>, TwsError> {
        self.up()?;
        let all = self.executions.lock().unwrap().clone();
        Ok(all.into_iter().filter(|e| !matches!(e, TwsEvent::Execution(x) if x.account != account)).collect())
    }

    async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError> {
        self.up()?;
        if let Some(found) = self.details.lock().unwrap().get(q) {
            return Ok(found.clone());
        }
        Ok(self
            .contracts
            .lock()
            .unwrap()
            .iter()
            .filter(|c| {
                c.sec_type == q.sec_type
                    && q.symbol.as_ref().is_none_or(|s| *s == c.symbol)
                    && q.exchange.as_ref().is_none_or(|e| *e == c.exchange)
                    && q.currency.as_ref().is_none_or(|u| *u == c.currency)
                    && q.con_id.is_none_or(|i| i == c.con_id)
            })
            .cloned()
            .collect())
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

    fn fut(con_id: i64, local: &str) -> TwsContract {
        TwsContract {
            con_id,
            symbol: "ES".into(),
            exchange: "CME".into(),
            primary_exchange: String::new(),
            currency: "USD".into(),
            min_tick: 0.25,
            lot_size: None,
            name: "E-mini S&P 500".into(),
            isin: None,
            sec_type: SecType::Fut,
            local_symbol: local.into(),
            multiplier: 50.0,
            last_trade: None,
            last_trade_at: None,
            strike: None,
            right: None,
        }
    }

    #[tokio::test]
    async fn fut_query_asks_for_fut_on_cme() {
        let fake = FakeTws::new();
        fake.add_contract(fut(1, "ESZ6"));
        fake.add_contract(fut(2, "ESH7"));
        let q = ContractQuery { symbol: Some("ES".into()), exchange: Some("CME".into()), sec_type: SecType::Fut, ..Default::default() };
        let found = fake.contract_details(&q).await.unwrap();
        assert_eq!(found.iter().map(|c| c.local_symbol.as_str()).collect::<Vec<_>>(), ["ESZ6", "ESH7"]);
        assert!(found.iter().all(|c| c.sec_type == SecType::Fut));
        // A stock query does not see futures.
        let stk = ContractQuery { symbol: Some("ES".into()), ..Default::default() };
        assert!(fake.contract_details(&stk).await.unwrap().is_empty());
    }

    #[test]
    fn last_trade_converts_contract_zone_to_utc() {
        let utc = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(last_trade_instant("20261218", Some("08:30"), "US/Central"), Some(utc("2026-12-18T14:30:00Z")));
        assert_eq!(last_trade_instant("20261218", None, "US/Central"), None);
        assert_eq!(last_trade_instant("202612", Some("08:30"), "US/Central"), None);
        assert_eq!(last_trade_instant("20261218", Some("08:30"), "Mars/Olympus"), None);
    }

    #[test]
    fn ib_contract_for_fut_uses_exchange_not_smart() {
        let c = ib_contract(1, SecType::Fut, "CME", "", "USD").unwrap();
        assert_eq!(c.security_type.as_str(), "FUT");
        assert_eq!(c.exchange.to_string(), "CME");
        let s = ib_contract(265598, SecType::Stk, "SMART", "NASDAQ", "USD").unwrap();
        assert_eq!(s.security_type.as_str(), "STK");
    }

    #[test]
    fn sec_type_round_trips() {
        for t in [SecType::Stk, SecType::Fut, SecType::Opt, SecType::Bag] {
            assert_eq!(SecType::parse(t.as_ibkr()), Some(t));
        }
        assert_eq!(SecType::parse("CASH"), None);
    }

    #[test]
    fn con_ids_beyond_i32_are_refused() {
        assert_eq!(con_id_i32(265598), Ok(265598));
        assert!(matches!(con_id_i32(i64::from(i32::MAX) + 1), Err(TwsError::Other(_))));
    }
}
