//! Orders, cancels, order lookups and previous closes over the TWS API.
//!
//! The adapter exists before IB Gateway is up: the supervisor (see
//! [`super::stream`]) swaps a connected client into its [`ClientSlot`], and
//! every call made while the slot is empty or the client is disconnected is a
//! transient `BrokerError::Network`, never queued.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use tracing::{debug, info};

use crate::adapter::{BrokerAdapter, BrokerError, BrokerOrderRequest, BrokerOrderResponse};
use crate::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState};
use crate::md::daily_stats::DailyStats;

use super::client::{ContractQuery, OrderKind, Side, Tif, TwsClient, TwsContract, TwsError, TwsOrder, TwsOrderState};

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
}

impl IbkrTwsAdapter {
    pub fn new(client: ClientSlot) -> Self {
        Self { client, contracts: Mutex::default(), symbols: Mutex::default(), refs: Mutex::default(), rejected_on_submit: Mutex::default() }
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

    async fn cancel_order(&self, external_order_id: &str, _symbol: &str) -> Result<(), BrokerError> {
        let client = self.client()?;
        let id: i32 = external_order_id
            .parse()
            .map_err(|_| BrokerError::NoSuchOrder(format!("not an IBKR order id: {external_order_id}")))?;
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
        let by_id = || external_order_id.parse::<i32>().ok().and_then(|id| all.iter().find(|(i, _, _)| *i == id));
        let (_, _, s) = by_ref
            .or_else(by_id)
            .ok_or_else(|| BrokerError::NoSuchOrder(format!("IBKR has no order {client_order_id} / {external_order_id}")))?;
        Ok(BrokerOrderState { outcome: outcome(&s.status), executed_qty: s.filled, avg_px: s.avg_fill_price })
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
}
