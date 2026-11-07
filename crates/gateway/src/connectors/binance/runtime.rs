//! Binance Spot's implementation of the rich runtime contract: instrument
//! rules, balances, open orders, paginated executions, lookup and the user-data
//! stream. Parsers are pure so recovery tests need no network.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use reqwest::Method;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::info;

use crate::adapter::{
    BrokerAdapter, BrokerError, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse,
};
use crate::contract::{BrokerOpenOrder, BrokerOrderState};
use crate::runtime::{
    AccountIdentity, AssetBalance, ExecutionCursor, ExecutionPage, Fee, InstrumentRules,
    Operation, RuntimeOrder, RuntimeOrderState, RuntimeVenue, Side, StreamState, SymbolCursor,
    Trade, TradeKey, TradeProvenance, VenueCapabilities, VenueEvent,
};

use super::adapter::BinanceAdapter;

pub const TRADE_PAGE_LIMIT: usize = 1_000;

#[derive(Clone)]
pub struct BinanceRuntimeVenue {
    adapter: Arc<BinanceAdapter>,
    identity: AccountIdentity,
}

impl BinanceRuntimeVenue {
    /// Bind an adapter to the operator-verified account identity. Nothing is
    /// inferred from an API response.
    pub fn new(adapter: Arc<BinanceAdapter>, identity: AccountIdentity) -> Result<Self, BrokerError> {
        if !identity.venue.eq_ignore_ascii_case("BINANCE") {
            return Err(BrokerError::NotConfigured(format!(
                "identity venue {} is not BINANCE",
                identity.venue
            )));
        }
        if identity.account_id.is_nil() {
            return Err(BrokerError::NotConfigured("account identity has no account id".into()));
        }
        if identity.external_account_ref.trim().is_empty() {
            return Err(BrokerError::NotConfigured(
                "account identity has no operator-verified external reference".into(),
            ));
        }
        if identity.environment != "LIVE" && identity.environment != "PAPER" {
            return Err(BrokerError::NotConfigured(format!(
                "unsupported Binance environment {}",
                identity.environment
            )));
        }
        Ok(Self { adapter, identity })
    }

    pub fn identity(&self) -> &AccountIdentity {
        &self.identity
    }
}

#[async_trait::async_trait]
impl BrokerAdapter for BinanceRuntimeVenue {
    async fn submit_order(&self, req: &BrokerOrderRequest) -> Result<BrokerOrderResponse, BrokerError> {
        self.adapter.submit_order(req).await
    }

    async fn cancel_order(&self, external_order_id: &str, symbol: &str) -> Result<(), BrokerError> {
        self.adapter.cancel_order(external_order_id, symbol).await
    }

    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        self.adapter.open_orders().await
    }

    async fn order_status(
        &self,
        client_order_id: &str,
        external_order_id: &str,
        symbol: &str,
    ) -> Result<BrokerOrderState, BrokerError> {
        self.adapter.order_status(client_order_id, external_order_id, symbol).await
    }
}

#[async_trait::async_trait]
impl RuntimeVenue for BinanceRuntimeVenue {
    async fn metadata(&self, symbols: &[String]) -> Result<Vec<InstrumentRules>, BrokerError> {
        let instruments = self.adapter.list_spot_instruments().await?;
        Ok(instruments
            .into_iter()
            .filter(|i| {
                symbols.is_empty()
                    || symbols.iter().any(|s| s == &i.broker_symbol || s == &i.definition.symbol)
            })
            .map(rules_from_instrument)
            .collect())
    }

    async fn balances(&self) -> Result<Vec<AssetBalance>, BrokerError> {
        let resp = self.adapter.signed(Method::GET, "/api/v3/account", &[]).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_balances(&body))
    }

    async fn snapshot_orders(&self) -> Result<Vec<RuntimeOrder>, BrokerError> {
        let resp = self.adapter.signed(Method::GET, "/api/v3/openOrders", &[]).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Err(crate::adapter::http_failure(status, resp.text().await.unwrap_or_default()));
        }
        let body: Value = resp.json().await.map_err(|e| BrokerError::Network(e.to_string()))?;
        Ok(parse_runtime_orders(&body))
    }

    async fn executions(&self, cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError> {
        let mut trades = Vec::new();
        let mut next = cursor.clone();
        let mut complete = true;
        let mut missing = Vec::new();

        for (symbol, position) in &cursor.positions {
            if position.last_trade_id.is_none() && position.start_time.is_none() {
                complete = false;
                missing.push(symbol.clone());
                continue;
            }
            let from_id = position.last_trade_id.map(|id| id.saturating_add(1));
            let start_ms = position.start_time.map(|t| t.timestamp_millis());
            let body = match self.adapter.my_trades_body(symbol, from_id, start_ms).await {
                Ok(body) => body,
                Err(e) => {
                    complete = false;
                    missing.push(symbol.clone());
                    info!(symbol, error = %e, "Binance execution history unavailable");
                    continue;
                }
            };
            let rows = trade_rows(&body);
            let page = parse_trade_rows(&body, &self.identity, TradeProvenance::Recovery);
            let (advanced, more) = advance_symbol_cursor(position, &page, rows.len());
            next.positions.insert(symbol.clone(), advanced);
            if more {
                complete = false;
            }
            trades.extend(page);
        }

        Ok(ExecutionPage { trades, next, complete, missing })
    }

    async fn lookup(&self, client_id: &str, symbol: &str) -> Result<RuntimeOrder, BrokerError> {
        let order = self.adapter.get_order_by(symbol, client_id, "").await?;
        parse_runtime_order(&order)
            .ok_or_else(|| BrokerError::BrokerRejected("unparseable Binance order".into()))
    }

    async fn subscribe(&self, tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError> {
        let identity = self.identity.clone();
        let adapter = self.adapter.clone();
        tokio::spawn(async move {
            super::stream::runtime(identity, adapter, tx).await;
        });
        Ok(())
    }
}

fn rules_from_instrument(instrument: BrokerInstrument) -> InstrumentRules {
    let definition = instrument.definition;
    InstrumentRules {
        symbol: definition.symbol.clone(),
        venue: definition.venue.clone(),
        venue_symbol: instrument.broker_symbol,
        base_asset: definition
            .native_id
            .clone()
            .unwrap_or_else(|| definition.symbol.clone()),
        quote_asset: definition.currency.clone(),
        tick_size: definition.price_increment,
        step_size: definition.size_increment,
        multiplier: definition.contract_size,
        min_quantity: instrument.min_quantity,
        max_quantity: instrument.max_quantity,
        min_notional: instrument.min_notional,
        max_notional: instrument.max_notional,
        operations: vec![Operation::Place, Operation::Cancel],
        capabilities: VenueCapabilities {
            cancel_on_disconnect: false,
            atomic_cancel_replace: false,
            open_orders_snapshot: true,
            order_lookup: true,
            trade_history: true,
            balance_snapshot: true,
        },
    }
}

pub(crate) fn parse_balances(body: &Value) -> Vec<AssetBalance> {
    body["balances"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(AssetBalance {
                asset: b["asset"].as_str()?.to_string(),
                free: b["free"].as_str()?.parse().ok()?,
                locked: b["locked"].as_str()?.parse().ok()?,
            })
        })
        .collect()
}

pub(crate) fn parse_runtime_orders(body: &Value) -> Vec<RuntimeOrder> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_runtime_order)
        .collect()
}

fn parse_runtime_order(order: &Value) -> Option<RuntimeOrder> {
    let status = order["status"].as_str().unwrap_or("");
    let original: f64 = order["origQty"].as_str()?.parse().ok()?;
    let executed: f64 = order["executedQty"].as_str()?.parse().ok()?;
    let limit_price = if order["type"].as_str() == Some("MARKET") {
        None
    } else {
        order["price"].as_str().and_then(|p| p.parse().ok())
    };
    Some(RuntimeOrder {
        client_order_id: order["clientOrderId"].as_str()?.to_string(),
        venue_order_id: order["orderId"]
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| order["orderId"].as_str().map(str::to_string))?,
        symbol: order["symbol"].as_str()?.to_string(),
        side: side(order["side"].as_str()?)?,
        original_quantity: original,
        executed_quantity: executed,
        leaves_quantity: (original - executed).max(0.0),
        limit_price,
        state: runtime_state(status),
        original_time: millis(order, "time")
            .or_else(|| millis(order, "updateTime"))
            .unwrap_or_else(unix_epoch),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawTrade {
    pub trade_id: String,
    pub venue_order_id: String,
    pub client_order_id: String,
    pub symbol: String,
    pub side: Side,
    pub quantity: f64,
    pub price: f64,
    pub executed_at: DateTime<Utc>,
    pub fee: Option<Fee>,
}

/// Normalize one WebSocket `executionReport` trade. Both the follow path and
/// the runtime path consume this, so fees and timestamps cannot drift.
pub(crate) fn parse_raw_trade(event: &Value) -> Option<RawTrade> {
    let trade_id = int_string(&event["t"])
        .or_else(|| int_string(&event["i"]))?;
    let client = match event["C"].as_str() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => event["c"].as_str().unwrap_or("").to_string(),
    };
    Some(RawTrade {
        trade_id,
        venue_order_id: int_string(&event["i"]).unwrap_or_default(),
        client_order_id: client,
        symbol: event["s"].as_str()?.to_string(),
        side: side(event["S"].as_str()?)?,
        quantity: event["l"].as_str()?.parse().ok()?,
        price: event["L"].as_str()?.parse().ok()?,
        executed_at: millis(event, "T")
            .or_else(|| millis(event, "E"))
            .unwrap_or_else(unix_epoch),
        fee: fee_from(event["n"].as_str(), event["N"].as_str()),
    })
}

pub(crate) fn parse_raw_order(event: &Value, state: RuntimeOrderState) -> Option<RuntimeOrder> {
    let original: f64 = event["q"].as_str().and_then(|q| q.parse().ok()).unwrap_or(0.0);
    let executed: f64 = event["z"].as_str().and_then(|z| z.parse().ok()).unwrap_or(0.0);
    let client = match event["C"].as_str() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => event["c"].as_str().unwrap_or("").to_string(),
    };
    Some(RuntimeOrder {
        client_order_id: client,
        venue_order_id: int_string(&event["i"]).unwrap_or_default(),
        symbol: event["s"].as_str()?.to_string(),
        side: side(event["S"].as_str()?)?,
        original_quantity: original,
        executed_quantity: executed,
        leaves_quantity: (original - executed).max(0.0),
        limit_price: if event["o"].as_str() == Some("MARKET") {
            None
        } else {
            event["p"].as_str().and_then(|p| p.parse().ok())
        },
        state,
        original_time: millis(event, "O")
            .or_else(|| millis(event, "T"))
            .unwrap_or_else(unix_epoch),
    })
}

pub(crate) fn runtime_events(
    event: &Value,
    identity: &AccountIdentity,
    provenance: TradeProvenance,
) -> Vec<VenueEvent> {
    let mut out = Vec::new();
    match event["x"].as_str().unwrap_or("") {
        "NEW" => {
            if let Some(order) = parse_raw_order(event, runtime_state(event["X"].as_str().unwrap_or("NEW"))) {
                out.push(VenueEvent::Order(order));
            }
        }
        "TRADE" => {
            if let Some(raw) = parse_raw_trade(event) {
                out.push(VenueEvent::Trade(Trade {
                    key: TradeKey {
                        venue: identity.venue.clone(),
                        account_id: identity.account_id,
                        environment: identity.environment.clone(),
                        symbol: raw.symbol.clone(),
                        trade_id: raw.trade_id,
                    },
                    venue_order_id: raw.venue_order_id,
                    client_order_id: raw.client_order_id,
                    side: raw.side,
                    quantity: raw.quantity,
                    price: raw.price,
                    executed_at: raw.executed_at,
                    fee: raw.fee,
                    provenance,
                }));
            }
        }
        status @ ("CANCELED" | "REJECTED" | "EXPIRED") => {
            let state = match status {
                "CANCELED" => RuntimeOrderState::Canceled,
                "REJECTED" => RuntimeOrderState::Rejected,
                _ => RuntimeOrderState::Expired,
            };
            if let Some(order) = parse_raw_order(event, state) {
                out.push(VenueEvent::Order(order));
            }
        }
        _ => {}
    }
    out
}

pub(crate) fn parse_trade_rows(
    body: &Value,
    identity: &AccountIdentity,
    provenance: TradeProvenance,
) -> Vec<Trade> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let trade_id = int_string(&t["id"])?;
            Some(Trade {
                key: TradeKey {
                    venue: identity.venue.clone(),
                    account_id: identity.account_id,
                    environment: identity.environment.clone(),
                    symbol: t["symbol"].as_str()?.to_string(),
                    trade_id,
                },
                venue_order_id: int_string(&t["orderId"]).unwrap_or_default(),
                client_order_id: String::new(),
                side: if t["isBuyer"].as_bool()? { Side::Buy } else { Side::Sell },
                quantity: t["qty"].as_str()?.parse().ok()?,
                price: t["price"].as_str()?.parse().ok()?,
                executed_at: millis(t, "time").unwrap_or_else(unix_epoch),
                fee: fee_from(t["commission"].as_str(), t["commissionAsset"].as_str()),
                provenance,
            })
        })
        .collect()
}

fn trade_rows(body: &Value) -> &[Value] {
    body.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Advance one symbol's watermark. A full page is treated as possibly
/// incomplete, because the venue exposes no "there is more" flag.
pub(crate) fn advance_symbol_cursor(
    current: &SymbolCursor,
    trades: &[Trade],
    page_len: usize,
) -> (SymbolCursor, bool) {
    let last = trades
        .iter()
        .filter_map(|t| t.key.trade_id.parse::<i64>().ok())
        .max();
    let last_trade_id = last
        .map(|id| id.saturating_add(1))
        .or(current.last_trade_id);
    let start_time = if last.is_some() { None } else { current.start_time };
    (
        SymbolCursor { last_trade_id, start_time },
        page_len >= TRADE_PAGE_LIMIT,
    )
}

fn fee_from(amount: Option<&str>, currency: Option<&str>) -> Option<Fee> {
    Some(Fee {
        amount: amount?.parse().ok()?,
        currency: currency?.to_string(),
    })
}

fn side(raw: &str) -> Option<Side> {
    match raw {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

fn runtime_state(status: &str) -> RuntimeOrderState {
    match status {
        "NEW" => RuntimeOrderState::New,
        "PARTIALLY_FILLED" => RuntimeOrderState::PartiallyFilled,
        "FILLED" => RuntimeOrderState::Filled,
        "CANCELED" => RuntimeOrderState::Canceled,
        "REJECTED" => RuntimeOrderState::Rejected,
        "EXPIRED" => RuntimeOrderState::Expired,
        _ => RuntimeOrderState::Unknown,
    }
}

fn int_string(value: &Value) -> Option<String> {
    value
        .as_i64()
        .map(|i| i.to_string())
        .or_else(|| value.as_str().map(str::to_string))
}

fn millis(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value[key].as_i64()?).single()
}

fn unix_epoch() -> DateTime<Utc> {
    Utc.timestamp_millis_opt(0).single().expect("epoch is valid")
}

pub(crate) fn health_event(state: StreamState) -> VenueEvent {
    VenueEvent::Health { state, observed_at: Utc::now() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::binance::adapter::{lookup_error, parse_exchange_info};
    use uuid::Uuid;

    fn identity() -> AccountIdentity {
        AccountIdentity {
            account_id: Uuid::new_v4(),
            venue: "BINANCE".into(),
            environment: "LIVE".into(),
            external_account_ref: "sub-1".into(),
        }
    }

    #[test]
    fn lifecycle_reports_all_outcomes() {
        let id = identity();
        let new = serde_json::json!({
            "x": "NEW", "X": "NEW", "c": "c-1", "i": 9, "s": "BTCUSDT", "S": "BUY",
            "q": "10", "z": "0", "p": "100", "o": "LIMIT", "O": 1_700_000_000_000i64,
        });
        assert!(matches!(
            runtime_events(&new, &id, TradeProvenance::Stream).as_slice(),
            [VenueEvent::Order(RuntimeOrder { state: RuntimeOrderState::New, .. })]
        ));
        let traded = serde_json::json!({
            "x": "TRADE", "X": "PARTIALLY_FILLED", "c": "c-1", "i": 9, "s": "BTCUSDT",
            "S": "BUY", "l": "4", "L": "100", "t": 7, "T": 1_700_000_000_100i64,
            "n": "0.01", "N": "BNB",
        });
        let events = runtime_events(&traded, &id, TradeProvenance::Stream);
        let VenueEvent::Trade(trade) = &events[0] else { panic!("a trade") };
        assert_eq!((trade.quantity, trade.price), (4.0, 100.0));
        assert_eq!(trade.fee, Some(Fee { amount: 0.01, currency: "BNB".into() }));
        for (raw, expected) in [
            ("CANCELED", RuntimeOrderState::Canceled),
            ("REJECTED", RuntimeOrderState::Rejected),
            ("EXPIRED", RuntimeOrderState::Expired),
        ] {
            let event = serde_json::json!({
                "x": raw, "c": "c-1", "i": 9, "s": "BTCUSDT", "S": "SELL",
                "q": "10", "z": "4", "p": "100", "o": "LIMIT",
            });
            let events = runtime_events(&event, &id, TradeProvenance::Stream);
            assert!(matches!(
                events.as_slice(),
                [VenueEvent::Order(RuntimeOrder { state, leaves_quantity: 6.0, .. })] if *state == expected
            ));
        }
    }

    #[test]
    fn partial_fill_keeps_remaining_quantity() {
        let order = serde_json::json!({
            "clientOrderId": "c-1", "orderId": 9, "symbol": "BTCUSDT", "side": "BUY",
            "origQty": "10", "executedQty": "4", "price": "100", "status": "PARTIALLY_FILLED",
            "type": "LIMIT", "time": 1_700_000_000_000i64,
        });
        let parsed = parse_runtime_order(&order).expect("order");
        assert_eq!(parsed.leaves_quantity, 6.0);
        assert_eq!(parsed.original_time.timestamp_millis(), 1_700_000_000_000);
    }

    #[test]
    fn same_trade_id_on_two_accounts_and_symbols_stays_distinct() {
        let a = identity();
        let b = identity();
        let row = |symbol: &str| serde_json::json!({
            "id": 7, "orderId": 9, "symbol": symbol, "isBuyer": true,
            "qty": "1", "price": "2", "time": 1_700_000_000_000i64,
        });
        let btc = parse_trade_rows(&serde_json::json!([row("BTCUSDT")]), &a, TradeProvenance::Recovery);
        let eth = parse_trade_rows(&serde_json::json!([row("ETHUSDT")]), &b, TradeProvenance::Recovery);
        assert_eq!(btc[0].key.trade_id, eth[0].key.trade_id);
        assert_ne!(btc[0].key, eth[0].key);
    }

    #[test]
    fn a_full_trade_page_is_incomplete_and_advances_the_cursor() {
        let rows: Vec<Value> = (0..TRADE_PAGE_LIMIT)
            .map(|i| serde_json::json!({
                "id": i, "orderId": 9, "symbol": "BTCUSDT", "isBuyer": true,
                "qty": "1", "price": "2", "time": 1_700_000_000_000i64,
            }))
            .collect();
        let body = Value::Array(rows);
        let trades = parse_trade_rows(&body, &identity(), TradeProvenance::Recovery);
        let (next, more) = advance_symbol_cursor(&SymbolCursor::default(), &trades, TRADE_PAGE_LIMIT);
        assert!(more);
        assert_eq!(next.last_trade_id, Some(TRADE_PAGE_LIMIT as i64));
        assert!(next.start_time.is_none());
    }

    #[test]
    fn market_orders_have_no_limit_price_and_filters_survive() {
        let body = serde_json::json!({ "symbols": [{
            "symbol": "SOLBTC", "baseAsset": "SOL", "quoteAsset": "BTC", "status": "TRADING",
            "quoteAssetPrecision": 8,
            "filters": [
                { "filterType": "PRICE_FILTER", "tickSize": "0.0000001" },
                { "filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "9000" },
                { "filterType": "NOTIONAL", "minNotional": "0.0001" }
            ]
        }]});
        let rules = rules_from_instrument(parse_exchange_info(&body).remove(0));
        assert_eq!((rules.tick_size, rules.step_size), (0.0000001, 0.001));
        assert_eq!((rules.min_quantity, rules.max_quantity), (Some(0.001), Some(9000.0)));
        assert_eq!(rules.min_notional, Some(0.0001));
        assert!(!rules.capabilities.cancel_on_disconnect);
        assert!(!rules.capabilities.atomic_cancel_replace);
        assert!(!rules.operations.contains(&Operation::Amend));

        let market = serde_json::json!({
            "x": "NEW", "X": "NEW", "c": "c-1", "i": 9, "s": "BTCUSDT", "S": "BUY",
            "q": "1", "z": "0", "p": "0", "o": "MARKET",
        });
        let events = runtime_events(&market, &identity(), TradeProvenance::Stream);
        assert!(matches!(
            events.as_slice(),
            [VenueEvent::Order(RuntimeOrder { limit_price: None, .. })]
        ));
    }

    #[test]
    fn an_unknown_client_id_is_no_such_order() {
        assert!(matches!(
            lookup_error(r#"{"code":-2013,"msg":"Order does not exist."}"#.into()),
            BrokerError::NoSuchOrder(_)
        ));
    }

    #[test]
    fn balances_and_fees_keep_third_asset_units() {
        let body = serde_json::json!({
            "balances": [
                { "asset": "USDT", "free": "900", "locked": "100" },
                { "asset": "BNB", "free": "0.01", "locked": "0" }
            ]
        });
        let balances = parse_balances(&body);
        assert_eq!(balances[0].total(), 1_000.0);
        assert_eq!(balances[1].asset, "BNB");

        let rows = serde_json::json!([{
            "id": 1, "orderId": 2, "symbol": "BTCUSDT", "isBuyer": true,
            "qty": "1", "price": "100", "time": 1_700_000_000_000i64,
            "commission": "0.01", "commissionAsset": "BNB"
        }]);
        let trades = parse_trade_rows(&rows, &identity(), TradeProvenance::History);
        assert_eq!(trades[0].fee, Some(Fee { amount: 0.01, currency: "BNB".into() }));
    }

    #[test]
    fn stream_health_distinguishes_stale_from_closed() {
        assert_ne!(health_event(StreamState::Stale), health_event(StreamState::Closed));
        assert!(matches!(health_event(StreamState::Stale), VenueEvent::Health { state: StreamState::Stale, .. }));
    }
}
