//! Orders, cancels, order lookups and previous closes over the TWS API.
//!
//! The adapter exists before IB Gateway is up: the supervisor (see
//! [`super::stream`]) swaps a connected client into its [`ClientSlot`], and
//! every call made while the slot is empty or the client is disconnected is a
//! transient `BrokerError::Network`, never queued.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use crate::adapter::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};
use crate::md::daily_stats::DailyStats;
use crate::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueFill, VenueOrderStatus, VenueOrderUpdate};

use super::client::{ContractQuery, OrderKind, Side, Tif, TwsClient, TwsContract, TwsError, TwsEvent, TwsExecution, TwsOrder, TwsOrderState};

/// How far back `reqExecutions` reaches.
const MAX_HISTORY_DAYS: i64 = 7;

/// The connected client, once there is one.
pub type ClientSlot = Arc<RwLock<Option<Arc<dyn TwsClient>>>>;

const RECONNECTING: &str = "IBKR gateway reconnecting; retry";

/// How an IB error code reads for an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Reject,
    Transient,
    NoSuchOrder,
    Info,
}

/// IB error codes: below 2000 are errors, 2000 and up are notices.
pub fn map_error(code: i32, _msg: &str) -> ErrorClass {
    match code {
        201 => ErrorClass::Reject,
        202 => ErrorClass::Info, // order cancelled; the status update books it
        135 | 10147 | 10148 => ErrorClass::NoSuchOrder,
        1100 | 1101 | 1102 | 2110 | 504 => ErrorClass::Transient,
        2104 | 2106 | 2158 => ErrorClass::Info,
        c if c < 2000 => ErrorClass::Reject,
        _ => ErrorClass::Info,
    }
}

fn broker_error(e: TwsError) -> BrokerError {
    match e {
        TwsError::NotConnected => BrokerError::Network(RECONNECTING.into()),
        TwsError::Api { code, message } => match map_error(code, &message) {
            ErrorClass::NoSuchOrder => BrokerError::NoSuchOrder(format!("IB error {code}: {message}")),
            ErrorClass::Reject => BrokerError::BrokerRejected(format!("IB error {code}: {message}")),
            // A notice says nothing about the order's fate: unknown, so recon settles it.
            ErrorClass::Transient | ErrorClass::Info => BrokerError::Network(format!("IB error {code}: {message}")),
        },
        // Timeouts and decode failures say nothing about whether IB took the request.
        TwsError::Other(m) => BrokerError::Network(m),
    }
}

/// How IB's order status string ends (or doesn't) an order.
pub(crate) fn outcome(status: &str) -> BrokerOrderOutcome {
    match status {
        "Filled" => BrokerOrderOutcome::Filled,
        "Cancelled" | "ApiCancelled" => BrokerOrderOutcome::Canceled,
        "Inactive" => BrokerOrderOutcome::Rejected,
        _ => BrokerOrderOutcome::Working,
    }
}

pub struct IbkrTwsAdapter {
    client: ClientSlot,
    /// Contract details per conId: routing facts don't change intraday.
    contracts: Mutex<HashMap<i64, TwsContract>>,
    /// Ticker -> conId, learned from orders and lookups, for previous closes.
    symbols: Mutex<HashMap<String, i64>>,
    /// IBKR order id -> OMS order id (the order ref), for events that carry only the id.
    refs: Mutex<HashMap<i32, String>>,
    /// IBKR ids whose reject `submit_order` already returned: the streamed copy is not booked again.
    rejected_on_submit: Mutex<HashSet<i32>>,
    /// Orders this adapter placed, as an order state, for a reject that
    /// arrives before IBKR reports the order back.
    placed: Mutex<HashMap<i32, VenueOrderUpdate>>,
    /// Exec bases whose changed correction was already warned about; repeat pulls only debug-log it.
    warned_corrections: Mutex<HashSet<String>>,
}

impl IbkrTwsAdapter {
    pub fn new(client: ClientSlot) -> Self {
        Self {
            client,
            contracts: Mutex::default(),
            symbols: Mutex::default(),
            refs: Mutex::default(),
            rejected_on_submit: Mutex::default(),
            placed: Mutex::default(),
            warned_corrections: Mutex::default(),
        }
    }

    /// The connected client, or the transient error to return.
    fn client(&self) -> Result<Arc<dyn TwsClient>, BrokerError> {
        let c = self.client.read().unwrap().clone();
        match c {
            Some(c) if c.connected() => Ok(c),
            _ => Err(BrokerError::Network(RECONNECTING.into())),
        }
    }

    /// The OMS order id for an IBKR order id, when known.
    pub fn order_ref(&self, order_id: i32) -> Option<String> {
        self.refs.lock().unwrap().get(&order_id).cloned()
    }

    /// The client order id an IBKR event names: its order ref (an OMS order's
    /// UUID, or whatever another client set), else the ref learned for the id.
    pub(crate) fn client_ref(&self, order_id: i32, order_ref: &str) -> Option<String> {
        if !order_ref.is_empty() {
            return Some(order_ref.to_string());
        }
        self.order_ref(order_id).filter(|r| !r.is_empty())
    }

    /// The order state of an order this adapter placed.
    pub(crate) fn placed(&self, order_id: i32) -> Option<VenueOrderUpdate> {
        self.placed.lock().unwrap().get(&order_id).cloned()
    }

    pub(crate) fn rejected_on_submit(&self, order_id: i32) -> bool {
        self.rejected_on_submit.lock().unwrap().contains(&order_id)
    }

    /// Records IBKR id -> OMS id pairs from an order listing.
    pub(crate) fn learn(&self, orders: &[(i32, String, TwsOrderState)]) {
        let mut refs = self.refs.lock().unwrap();
        for (id, r, _) in orders {
            if !r.is_empty() {
                refs.insert(*id, r.clone());
            }
        }
    }

    async fn contract(&self, client: &dyn TwsClient, con_id: i64) -> Result<TwsContract, BrokerError> {
        if let Some(c) = self.contracts.lock().unwrap().get(&con_id) {
            return Ok(c.clone());
        }
        let q = ContractQuery { con_id: Some(con_id), ..ContractQuery::default() };
        let found = client.contract_details(&q).await.map_err(broker_error)?;
        let c = found
            .into_iter()
            .find(|c| c.con_id == con_id)
            .ok_or_else(|| BrokerError::BrokerRejected(format!("IBKR knows no contract {con_id}")))?;
        self.contracts.lock().unwrap().insert(con_id, c.clone());
        self.symbols.lock().unwrap().insert(c.symbol.clone(), con_id);
        Ok(c)
    }

    /// Every order IB reports, working ones first.
    async fn all_orders(&self, client: &dyn TwsClient) -> Result<Vec<(i32, String, TwsOrderState)>, BrokerError> {
        let mut all = client.open_orders().await.map_err(broker_error)?;
        all.extend(client.completed_orders().await.map_err(broker_error)?);
        self.learn(&all);
        Ok(all)
    }

    /// One unambiguous conId for a ticker, or `None`.
    async fn con_id_for(&self, client: &dyn TwsClient, symbol: &str) -> Option<i64> {
        if let Some(id) = self.symbols.lock().unwrap().get(symbol) {
            return Some(*id);
        }
        let q = ContractQuery { symbol: Some(symbol.to_string()), ..ContractQuery::default() };
        let found = client.contract_details(&q).await.ok()?;
        let first = found.first()?.con_id;
        if found.iter().any(|c| c.con_id != first) {
            debug!(symbol, "ticker is ambiguous at IBKR; no previous close");
            return None;
        }
        self.symbols.lock().unwrap().insert(symbol.to_string(), first);
        Some(first)
    }
}

fn parse_side(s: &str) -> Result<Side, BrokerError> {
    match s.to_ascii_lowercase().as_str() {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(BrokerError::BrokerRejected(format!("unsupported side: {other}"))),
    }
}

fn parse_tif(s: &str) -> Result<Tif, BrokerError> {
    match s.to_ascii_lowercase().as_str() {
        "day" => Ok(Tif::Day),
        "gtc" => Ok(Tif::Gtc),
        other => Err(BrokerError::BrokerRejected(format!("unsupported time in force for IBKR: {other}"))),
    }
}

fn parse_kind(req: &BrokerOrderRequest) -> Result<OrderKind, BrokerError> {
    match (req.order_type.as_str(), req.limit_price) {
        ("market", _) => Ok(OrderKind::Mkt),
        ("limit", Some(px)) => Ok(OrderKind::Lmt(px)),
        ("limit", None) => Err(BrokerError::BrokerRejected("a limit order needs a limit price".into())),
        (other, _) => Err(BrokerError::NotConfigured(format!("unsupported order type: {other}"))),
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for IbkrTwsAdapter {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        let con_id: i64 = req
            .native_id
            .as_deref()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BrokerError::BrokerRejected("instrument has no IBKR conId; import it first".into()))?;
        let (action, order_type, tif) = (parse_side(&req.side)?, parse_kind(req)?, parse_tif(&req.time_in_force)?);
        let client = self.client()?;
        let contract = self.contract(client.as_ref(), con_id).await?;
        let order_id = client.next_order_id().await.map_err(broker_error)?;
        let order = TwsOrder {
            order_id,
            con_id,
            exchange: "SMART".into(),
            primary_exchange: contract.primary_exchange.clone(),
            currency: contract.currency.clone(),
            action,
            qty: req.quantity,
            order_type,
            tif,
            order_ref: req.order_id.clone(),
            account: req.external_account_ref.clone(),
        };
        self.refs.lock().unwrap().insert(order_id, req.order_id.clone());
        let state = TwsOrderState {
            status: "PendingSubmit".into(),
            filled: 0.0,
            remaining: order.qty,
            avg_fill_price: 0.0,
            symbol: contract.symbol.clone(),
            perm_id: 0,
            order: order.clone(),
        };
        let mut placed = order_update(order_id, &state);
        placed.client_order_id = Some(req.order_id.clone());
        self.placed.lock().unwrap().insert(order_id, placed);
        info!(order_id, con_id, order_ref = %req.order_id, "placing IBKR order");
        client.place(&order).await.map_err(|e| {
            let e = broker_error(e);
            if matches!(e, BrokerError::BrokerRejected(_)) {
                self.rejected_on_submit.lock().unwrap().insert(order_id);
            }
            e
        })?;
        Ok(BrokerOrderResponse { external_order_id: order_id.to_string() })
    }

    /// The id is an OMS order's API order id, or (adopted / from the stream)
    /// an IBKR permId. It is resolved to an orderId this client may cancel
    /// first; an unresolved permId is never sent as an orderId.
    async fn cancel_order(&self, external_order_id: &str, _symbol: &str) -> Result<(), BrokerError> {
        let client = self.client()?;
        let open = client.open_orders().await.map_err(broker_error)?;
        self.learn(&open);
        let by_perm = external_order_id
            .parse::<i64>()
            .ok()
            .and_then(|p| open.iter().find(|(_, _, s)| s.perm_id != 0 && s.perm_id == p))
            .map(|(id, _, _)| *id);
        let own = || external_order_id.parse::<i32>().ok().filter(|id| self.order_ref(*id).is_some() || self.placed(*id).is_some());
        let id = by_perm.or_else(own).ok_or_else(|| {
            BrokerError::NoSuchOrder(format!(
                "IBKR order {external_order_id} is not an open order this API client can cancel \
                 (orders of other API clients cannot be cancelled by the OMS)"
            ))
        })?;
        client.cancel(id).await.map_err(broker_error)
    }

    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        let client = self.client()?;
        let open = client.open_orders().await.map_err(broker_error)?;
        self.learn(&open);
        Ok(open
            .into_iter()
            .filter_map(|(id, r, s)| {
                let client_order_id = if r.is_empty() { self.order_ref(id)? } else { r };
                Some(BrokerOpenOrder {
                    client_order_id,
                    venue_order_id: id.to_string(),
                    symbol: s.symbol,
                    executed_qty: s.filled,
                })
            })
            .collect())
    }

    async fn order_status(&self, client_order_id: &str, external_order_id: &str, _symbol: &str) -> Result<BrokerOrderState, BrokerError> {
        let client = self.client()?;
        let all = self.all_orders(client.as_ref()).await?;
        let by_ref = all.iter().find(|(_, r, _)| !client_order_id.is_empty() && r == client_order_id);
        // An adopted order's id is IBKR's permId; an OMS order's the API order id.
        let by_perm = || external_order_id.parse::<i64>().ok().and_then(|p| all.iter().find(|(_, _, s)| s.perm_id != 0 && s.perm_id == p));
        let by_id = || external_order_id.parse::<i32>().ok().and_then(|id| all.iter().find(|(i, _, _)| *i == id));
        let (_, _, s) = by_ref
            .or_else(by_perm)
            .or_else(by_id)
            .ok_or_else(|| BrokerError::NoSuchOrder(format!("IBKR has no order {client_order_id} / {external_order_id}")))?;
        Ok(BrokerOrderState { outcome: outcome(&s.status), executed_qty: s.filled, avg_px: s.avg_fill_price })
    }
}

/// `<base>.NN` -> `<base>`; IBKR revises a fill by bumping the last segment.
pub(crate) fn exec_base(exec_id: &str) -> &str {
    exec_id.rsplit_once('.').map_or(exec_id, |(base, _)| base)
}

/// The `.NN` revision of an exec id (hex), 0 without one.
fn exec_rev(exec_id: &str) -> u64 {
    exec_id.rsplit_once('.').and_then(|(_, r)| u64::from_str_radix(r, 16).ok()).unwrap_or(0)
}

/// IBKR's permId when known (stable across client ids), else the API order id.
fn venue_order_id(perm_id: i64, order_id: i32) -> String {
    if perm_id != 0 { perm_id.to_string() } else { order_id.to_string() }
}

/// A commission as a fee: `None` for TWS's "no figure yet" (`f64::MAX`) and for zero.
pub(crate) fn commission_fee(amount: f64, currency: String) -> Option<(f64, String)> {
    (amount != 0.0 && amount.is_finite() && amount.abs() < 1e300).then_some((amount, currency))
}

/// What to say when a corrected execution changes a fill already booked
/// (amending it is out of scope); `None` when qty and price agree.
pub(crate) fn correction_warning(base: &str, booked: (f64, f64), corrected: (f64, f64)) -> Option<String> {
    let differs = |a: f64, b: f64| (a - b).abs() > 1e-9;
    (differs(booked.0, corrected.0) || differs(booked.1, corrected.1)).then(|| {
        format!(
            "IBKR corrected execution {base}: qty {} -> {}, price {} -> {}; the booked fill is NOT amended",
            booked.0, corrected.0, booked.1, corrected.1
        )
    })
}

/// One execution as a fill under its exec base id, or the field it lacks.
pub(crate) fn fill_from_exec(e: &TwsExecution, client_order_id: Option<String>) -> Result<VenueFill, &'static str> {
    let missing = if e.exec_id.is_empty() {
        Some("execId")
    } else if e.account.is_empty() {
        Some("acctNumber")
    } else if e.symbol.is_empty() {
        Some("symbol")
    } else if !(e.qty.is_finite() && e.qty > 0.0) {
        Some("shares")
    } else if !(e.price.is_finite() && e.price > 0.0) {
        Some("price")
    } else {
        None
    };
    if let Some(m) = missing {
        return Err(m);
    }
    Ok(VenueFill {
        exec_id: exec_base(&e.exec_id).to_string(),
        venue_order_id: venue_order_id(e.perm_id, e.order_id),
        client_order_id,
        account_ref: e.account.clone(),
        symbol: e.symbol.clone(),
        buy: e.buy,
        qty: e.qty,
        price: e.price,
        fee: None,
        executed_at: Some(e.time),
    })
}

fn venue_status(status: &str) -> VenueOrderStatus {
    match status {
        "Filled" => VenueOrderStatus::Filled,
        "Cancelled" | "ApiCancelled" => VenueOrderStatus::Canceled,
        "Inactive" => VenueOrderStatus::Rejected { reason: "IBKR order inactive".into() },
        _ => VenueOrderStatus::Working,
    }
}

/// One IBKR order state as a venue order update; the caller sets `client_order_id`.
pub(crate) fn order_update(order_id: i32, st: &TwsOrderState) -> VenueOrderUpdate {
    let (order_type, limit_px) = match st.order.order_type {
        OrderKind::Mkt => ("market", None),
        OrderKind::Lmt(px) => ("limit", Some(px)),
    };
    VenueOrderUpdate {
        venue_order_id: venue_order_id(st.perm_id, order_id),
        client_order_id: (!st.order.order_ref.is_empty()).then(|| st.order.order_ref.clone()),
        account_ref: st.order.account.clone(),
        symbol: st.symbol.clone(),
        buy: st.order.action == Side::Buy,
        order_type: order_type.into(),
        qty: st.order.qty,
        limit_px,
        status: venue_status(&st.status),
        cum_qty: st.filled,
        ts: None,
    }
}

/// `reqExecutions` rows of `account` as fills: one per exec base (its highest
/// revision, so a corrected fill dedupes against the original), each with its
/// commission when IBKR reported one, none executed before `since`.
pub(crate) fn fills_from_executions(
    rows: Vec<TwsEvent>,
    account: &str,
    since: DateTime<Utc>,
    client_ref: impl Fn(i32, &str) -> Option<String>,
    warned: &mut HashSet<String>,
) -> Vec<VenueFill> {
    let mut fees: HashMap<String, (f64, String)> = HashMap::new();
    // (full exec id, fill) per base, in first-seen order.
    let mut kept: Vec<(String, VenueFill)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let e = match row {
            TwsEvent::Commission { exec_id, amount, currency } => {
                if let Some(f) = commission_fee(amount, currency) {
                    fees.insert(exec_id, f);
                }
                continue;
            }
            TwsEvent::Execution(e) if e.account == account => e,
            _ => continue,
        };
        let fill = match fill_from_exec(&e, client_ref(e.order_id, &e.order_ref)) {
            Ok(f) => f,
            Err(missing) => {
                warn!(exec_id = %e.exec_id, missing, "IBKR pulled execution skipped: field missing");
                continue;
            }
        };
        match index.get(&fill.exec_id) {
            None => {
                index.insert(fill.exec_id.clone(), kept.len());
                kept.push((e.exec_id.clone(), fill));
            }
            Some(&i) => {
                let (prev_id, prev) = &kept[i];
                if let Some(w) = correction_warning(&fill.exec_id, (prev.qty, prev.price), (fill.qty, fill.price)) {
                    if warned.insert(fill.exec_id.clone()) {
                        warn!(exec_id = %e.exec_id, "{w}");
                    } else {
                        debug!(exec_id = %e.exec_id, "{w}");
                    }
                }
                if exec_rev(&e.exec_id) > exec_rev(prev_id) {
                    kept[i] = (e.exec_id.clone(), fill);
                }
            }
        }
    }
    kept.into_iter()
        .filter(|(_, f)| f.executed_at.map_or(true, |t| t >= since))
        .map(|(id, mut f)| {
            f.fee = fees.get(&id).cloned().or_else(|| {
                fees.iter().find(|(k, _)| exec_base(k) == f.exec_id).map(|(_, v)| v.clone())
            });
            f
        })
        .collect()
}

#[async_trait::async_trait]
impl ExecutionSource for IbkrTwsAdapter {
    /// `reqExecutions` filtered by account and time: IBKR serves the last ~7
    /// days in one response with no paging, so this is one page and no cursor
    /// (ingest dedupes the overlap on exec id).
    async fn fills_since(&self, account_ref: &str, _scope: &str, _from: Option<&Cursor>, since: DateTime<Utc>) -> Result<PullPage, BrokerError> {
        let client = self.client()?;
        let window = since.max(Utc::now() - chrono::Duration::days(MAX_HISTORY_DAYS));
        let rows = client.executions(account_ref, window).await.map_err(broker_error)?;
        let mut warned = std::mem::take(&mut *self.warned_corrections.lock().unwrap());
        let fills = fills_from_executions(rows, account_ref, since, |id, r| self.client_ref(id, r), &mut warned);
        self.warned_corrections.lock().unwrap().extend(warned);
        Ok(PullPage { fills, next: Cursor(String::new()), more: false })
    }

    async fn open_order_snapshot(&self, account_ref: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        let client = self.client()?;
        let open = client.open_orders().await.map_err(broker_error)?;
        self.learn(&open);
        Ok(open
            .iter()
            .filter(|(_, _, s)| s.order.account == account_ref)
            .map(|(id, r, s)| {
                let mut u = order_update(*id, s);
                u.client_order_id = self.client_ref(*id, r);
                u
            })
            .collect())
    }

    async fn scopes(&self, _account_ref: &str) -> Result<Vec<String>, BrokerError> {
        Ok(vec![String::new()])
    }

    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 120, max_history: Some(chrono::Duration::days(MAX_HISTORY_DAYS)), per_symbol: false }
    }
}

#[async_trait::async_trait]
impl DailyStats for IbkrTwsAdapter {
    /// Previous closes from delayed data. Tickers IBKR can't pin to one conId are left out.
    async fn daily_stats(&self, symbols: &[String]) -> Result<Vec<(String, f64)>, BrokerError> {
        if symbols.is_empty() {
            return Ok(Vec::new());
        }
        let client = self.client()?;
        let mut out = Vec::new();
        for s in symbols {
            let Some(con_id) = self.con_id_for(client.as_ref(), s).await else { continue };
            if let Some(px) = client.prev_close(con_id).await.map_err(broker_error)? {
                out.push((s.clone(), px));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::client::FakeTws;
    use super::*;
    use uuid::Uuid;

    pub(crate) fn aapl() -> TwsContract {
        TwsContract {
            con_id: 265598,
            symbol: "AAPL".into(),
            exchange: "SMART".into(),
            primary_exchange: "NASDAQ".into(),
            currency: "USD".into(),
            min_tick: 0.01,
            lot_size: None,
            name: "APPLE INC".into(),
            isin: None,
        }
    }

    pub(crate) fn rig() -> (Arc<FakeTws>, Arc<IbkrTwsAdapter>) {
        let fake = Arc::new(FakeTws::new());
        fake.details
            .lock()
            .unwrap()
            .insert(ContractQuery { con_id: Some(265598), ..ContractQuery::default() }, vec![aapl()]);
        let slot: ClientSlot = Arc::new(RwLock::new(Some(fake.clone() as Arc<dyn TwsClient>)));
        (fake, Arc::new(IbkrTwsAdapter::new(slot)))
    }

    fn req(native_id: Option<&str>) -> BrokerOrderRequest {
        BrokerOrderRequest {
            order_id: "7d3c1a52-0000-4000-8000-000000000001".into(),
            symbol: "AAPL".into(),
            native_id: native_id.map(str::to_string),
            quantity: 10.0,
            side: "buy".into(),
            order_type: "limit".into(),
            time_in_force: "day".into(),
            limit_price: Some(190.5),
            external_account_ref: "DU123".into(),
        }
    }

    #[tokio::test]
    async fn an_order_goes_out_by_con_id_with_the_oms_id_as_order_ref() {
        let (fake, adapter) = rig();
        let resp = adapter.submit_order(&req(Some("265598"))).await.unwrap();
        assert_eq!(resp.external_order_id, "100");
        let placed = fake.placed.lock().unwrap().clone();
        assert_eq!(
            placed,
            vec![TwsOrder {
                order_id: 100,
                con_id: 265598,
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Buy,
                qty: 10.0,
                order_type: OrderKind::Lmt(190.5),
                tif: Tif::Day,
                order_ref: "7d3c1a52-0000-4000-8000-000000000001".into(),
                account: "DU123".into(),
            }]
        );
        // A second order takes a fresh id and reuses the cached contract.
        fake.details.lock().unwrap().clear();
        assert_eq!(adapter.submit_order(&req(Some("265598"))).await.unwrap().external_order_id, "101");
    }

    #[tokio::test]
    async fn an_order_without_a_con_id_is_rejected_before_sending() {
        let (fake, adapter) = rig();
        let err = adapter.submit_order(&req(None)).await.err().unwrap();
        assert!(matches!(err, BrokerError::BrokerRejected(ref m) if m == "instrument has no IBKR conId; import it first"));
        assert!(fake.placed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancel_while_disconnected_is_transient() {
        let (fake, adapter) = rig();
        fake.set_connected(false);
        let err = adapter.cancel_order("100", "AAPL").await.err().unwrap();
        assert!(matches!(err, BrokerError::Network(ref m) if m == "IBKR gateway reconnecting; retry"));
        assert!(fake.cancelled.lock().unwrap().is_empty());
        // No client yet at all: the same.
        let empty = IbkrTwsAdapter::new(Arc::new(RwLock::new(None)));
        assert!(matches!(empty.cancel_order("100", "AAPL").await, Err(BrokerError::Network(_))));
    }

    #[test]
    fn a_notice_on_place_is_an_unknown_outcome_not_a_reject() {
        let api = |code| broker_error(TwsError::Api { code, message: "x".into() });
        assert!(matches!(api(2109), BrokerError::Network(_)));
        assert!(matches!(api(202), BrokerError::Network(_)));
        assert!(matches!(api(201), BrokerError::BrokerRejected(_)));
        assert!(matches!(api(135), BrokerError::NoSuchOrder(_)));
    }

    #[tokio::test]
    async fn order_status_finds_by_order_ref_then_by_id() {
        let (fake, adapter) = rig();
        let state = |status: &str, ref_: &str| TwsOrderState {
            status: status.into(),
            filled: 10.0,
            remaining: 0.0,
            avg_fill_price: 190.0,
            symbol: "AAPL".into(),
            perm_id: 0,
            order: TwsOrder {
                order_id: 5,
                con_id: 265598,
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Buy,
                qty: 10.0,
                order_type: OrderKind::Mkt,
                tif: Tif::Day,
                order_ref: ref_.into(),
                account: "DU123".into(),
            },
        };
        fake.completed.lock().unwrap().push((5, "oms-a".into(), state("Filled", "oms-a")));
        fake.completed.lock().unwrap().push((6, "".into(), state("Cancelled", "")));
        let s = adapter.order_status("oms-a", "999", "AAPL").await.unwrap();
        assert_eq!(s.outcome, BrokerOrderOutcome::Filled);
        let s = adapter.order_status("oms-b", "6", "AAPL").await.unwrap();
        assert_eq!(s.outcome, BrokerOrderOutcome::Canceled);
        assert!(matches!(adapter.order_status("oms-c", "7", "AAPL").await, Err(BrokerError::NoSuchOrder(_))));
    }

    #[test]
    fn error_codes_map() {
        use ErrorClass::*;
        for (code, class) in [
            (201, Reject),
            (202, Info),
            (135, NoSuchOrder),
            (10147, NoSuchOrder),
            (10148, NoSuchOrder),
            (1100, Transient),
            (1101, Transient),
            (1102, Transient),
            (2110, Transient),
            (504, Transient),
            (2104, Info),
            (2106, Info),
            (2158, Info),
            (399, Reject),
            (1999, Reject),
            (2000, Info),
            (2999, Info),
        ] {
            assert_eq!(map_error(code, ""), class, "code {code}");
        }
    }

    fn ex(exec_id: &str, account: &str, qty: f64, price: f64, mins_ago: i64) -> TwsEvent {
        TwsEvent::Execution(TwsExecution {
            exec_id: exec_id.into(),
            order_id: 42,
            perm_id: 555,
            order_ref: "".into(),
            account: account.into(),
            symbol: "AAPL".into(),
            buy: false,
            qty,
            price,
            time: Utc::now() - chrono::Duration::minutes(mins_ago),
        })
    }

    #[test]
    fn corrected_execution_dedupes_to_one_fill_by_base_id() {
        let rows = vec![
            ex("0000e0d5.6551a1b2.01.01", "U1", 3.0, 190.0, 5),
            ex("0000e0d5.6551a1b2.01.02", "U1", 3.0, 190.0, 5),
            TwsEvent::Commission { exec_id: "0000e0d5.6551a1b2.01.02".into(), amount: 1.5, currency: "USD".into() },
        ];
        let fills = fills_from_executions(rows, "U1", DateTime::<Utc>::MIN_UTC, |_, _| None, &mut HashSet::new());
        assert_eq!(fills.len(), 1);
        // The base id: the same id the stream sent the original under.
        assert_eq!(fills[0].exec_id, "0000e0d5.6551a1b2.01");
        assert_eq!((fills[0].venue_order_id.as_str(), fills[0].account_ref.as_str()), ("555", "U1"));
        assert_eq!(fills[0].fee, Some((1.5, "USD".to_string())));
    }

    #[test]
    fn correction_with_changed_qty_warns() {
        let w = correction_warning("b", (3.0, 190.0), (2.0, 190.0)).expect("a changed qty warns");
        assert!(w.contains("qty 3 -> 2") && w.contains("NOT amended"), "{w}");
        assert!(correction_warning("b", (3.0, 190.0), (3.0, 191.0)).is_some());
        assert_eq!(correction_warning("b", (3.0, 190.0), (3.0, 190.0)), None);
        // The pull keeps the highest revision's figures.
        let rows = || vec![ex("b.02", "U1", 2.0, 190.0, 1), ex("b.01", "U1", 3.0, 190.0, 1)];
        let mut warned = HashSet::new();
        let fills = fills_from_executions(rows(), "U1", DateTime::<Utc>::MIN_UTC, |_, _| None, &mut warned);
        assert_eq!((fills.len(), fills[0].qty), (1, 2.0));
        // Warned once per base: a repeat pull only debug-logs it.
        assert!(warned.contains("b"));
        fills_from_executions(rows(), "U1", DateTime::<Utc>::MIN_UTC, |_, _| None, &mut warned);
        assert_eq!(warned.len(), 1);
    }

    #[tokio::test]
    async fn fills_since_pulls_one_account_and_drops_older_fills() {
        let (fake, adapter) = rig();
        let id = Uuid::new_v4();
        adapter.learn(&[(42, id.to_string(), sample_state(42, &id.to_string()))]);
        fake.executions.lock().unwrap().extend([
            ex("new.01", "U1", 1.0, 10.0, 1),
            ex("old.01", "U1", 1.0, 10.0, 60),
            ex("other.01", "U2", 1.0, 10.0, 1),
            ex("", "U1", 1.0, 10.0, 1),
        ]);
        let page = adapter.fills_since("U1", "", None, Utc::now() - chrono::Duration::minutes(30)).await.unwrap();
        assert_eq!(page.fills.iter().map(|f| f.exec_id.as_str()).collect::<Vec<_>>(), ["new"]);
        assert_eq!(page.fills[0].client_order_id.as_deref(), Some(id.to_string().as_str()));
        assert_eq!((page.more, page.next.clone()), (false, Cursor(String::new())));
        let h = adapter.hints();
        assert_eq!((h.intraday_secs, h.max_history, h.per_symbol), (120, Some(chrono::Duration::days(7)), false));
        assert_eq!(adapter.scopes("U1").await.unwrap(), vec![String::new()]);
    }

    fn sample_state(id: i32, order_ref: &str) -> TwsOrderState {
        TwsOrderState {
            status: "Submitted".into(),
            filled: 1.0,
            remaining: 9.0,
            avg_fill_price: 10.0,
            symbol: "AAPL".into(),
            perm_id: 4242,
            order: TwsOrder {
                order_id: id,
                con_id: 265598,
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Sell,
                qty: 10.0,
                order_type: OrderKind::Mkt,
                tif: Tif::Day,
                order_ref: order_ref.into(),
                account: "U1".into(),
            },
        }
    }

    #[tokio::test]
    async fn open_order_snapshot_is_per_account() {
        let (fake, adapter) = rig();
        let mut other = sample_state(8, "");
        other.order.account = "U2".into();
        fake.open.lock().unwrap().extend([(7, "x".to_string(), sample_state(7, "x")), (8, String::new(), other)]);
        let snap = adapter.open_order_snapshot("U1").await.unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!((snap[0].venue_order_id.as_str(), snap[0].client_order_id.as_deref(), snap[0].cum_qty), ("4242", Some("x"), 1.0));
        assert_eq!((snap[0].buy, snap[0].order_type.as_str(), snap[0].status.clone()), (false, "market", VenueOrderStatus::Working));
    }

    #[tokio::test]
    async fn cancel_by_perm_id_resolves_to_order_id() {
        let (fake, adapter) = rig();
        fake.open.lock().unwrap().push((-3, String::new(), sample_state(-3, "")));
        adapter.cancel_order("4242", "AAPL").await.unwrap();
        assert_eq!(*fake.cancelled.lock().unwrap(), vec![-3]);
        // An OMS order's own API order id still cancels directly.
        let id = adapter.submit_order(&req(Some("265598"))).await.unwrap().external_order_id;
        adapter.cancel_order(&id, "AAPL").await.unwrap();
        assert_eq!(*fake.cancelled.lock().unwrap(), vec![-3, id.parse::<i32>().unwrap()]);
    }

    #[tokio::test]
    async fn cancel_unknown_perm_id_is_no_such_order_not_a_blind_cancel() {
        let (fake, adapter) = rig();
        // 77 could be an unrelated orderId at IBKR: it must not be sent.
        for id in ["987654321", "77"] {
            assert!(matches!(adapter.cancel_order(id, "AAPL").await, Err(BrokerError::NoSuchOrder(_))), "{id}");
        }
        assert!(fake.cancelled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn order_status_finds_an_adopted_order_by_perm_id() {
        let (fake, adapter) = rig();
        fake.completed.lock().unwrap().push((-3, String::new(), sample_state(-3, "")));
        let s = adapter.order_status("4242", "4242", "AAPL").await.unwrap();
        assert_eq!(s.executed_qty, 1.0);
    }
}
