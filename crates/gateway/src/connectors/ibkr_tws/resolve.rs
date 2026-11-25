//! Turns user input (a CSV of symbols or conIds) into IBKR contracts, and a
//! contract into the OMS's instrument record.
//!
//! Identity is the IBKR conId. A symbol that matches more than one contract is
//! reported with every candidate, never guessed.

use dataprovider::{DerivativeDef, InstrumentDef, OptionKind};

use super::client::{ContractQuery, SecType, TwsClient, TwsContract};
use super::exchanges::{from_mic, to_mic};
use crate::adapter::BrokerInstrument;

/// One data row of an import CSV. `line` is 1-based, counting the header.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvRow {
    pub line: usize,
    pub symbol: Option<String>,
    /// An IBKR exchange code; a MIC in the file is normalised to it.
    pub exchange: Option<String>,
    pub currency: Option<String>,
    pub con_id: Option<i64>,
}

/// Parses `symbol, exchange, currency, conid` columns (any order, header
/// required, other columns ignored). Each row needs a conid or a symbol.
pub fn parse_csv(text: &str) -> Result<Vec<CsvRow>, String> {
    let mut lines = text.trim_start_matches('\u{feff}').lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    let (_, header) = lines.by_ref().find(|(_, l)| !l.is_empty()).ok_or("the CSV is empty")?;
    let names: Vec<String> = split_fields(header).iter().map(|h| h.to_lowercase().replace(['_', ' '], "")).collect();
    let col = |name: &str| names.iter().position(|n| n == name);
    let (symbol, exchange, currency, con_id) = (col("symbol"), col("exchange"), col("currency"), col("conid"));
    if symbol.is_none() && con_id.is_none() {
        return Err("the CSV header needs a symbol or a conid column".into());
    }

    let mut rows = Vec::new();
    for (line, raw) in lines {
        if raw.is_empty() {
            continue;
        }
        let fields = split_fields(raw);
        let get = |i: Option<usize>| i.and_then(|i| fields.get(i)).filter(|f| !f.is_empty()).cloned();
        let con_id = match get(con_id) {
            None => None,
            Some(v) => Some(v.parse::<i64>().map_err(|_| format!("line {line}: conid '{v}' is not a number"))?),
        };
        let symbol = get(symbol).map(|s| s.to_uppercase());
        if con_id.is_none() && symbol.is_none() {
            return Err(format!("line {line}: needs a symbol or a conid"));
        }
        let exchange = get(exchange).map(|e| {
            let e = e.to_uppercase();
            from_mic(&e).map(str::to_string).unwrap_or(e)
        });
        rows.push(CsvRow { line, symbol, exchange, currency: get(currency).map(|c| c.to_uppercase()), con_id });
    }
    Ok(rows)
}

/// Comma-split with double-quote support (`"A, B"`), trimmed.
fn split_fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for ch in line.chars() {
        match ch {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Found(TwsContract),
    Ambiguous(Vec<TwsContract>),
    NotFound(String),
}

/// Looks one row up at IB Gateway. A conId is exact; a symbol is narrowed by
/// the row's exchange (matched against the contract's primary exchange) and
/// currency. A contract on an exchange the OMS has no MIC for is not found.
pub async fn resolve(client: &dyn TwsClient, row: &CsvRow) -> Resolution {
    let query = ContractQuery {
        symbol: if row.con_id.is_some() { None } else { row.symbol.clone() },
        exchange: None,
        currency: row.currency.clone(),
        con_id: row.con_id,
        ..ContractQuery::default()
    };
    let mut found = match client.contract_details(&query).await {
        Ok(found) => found,
        Err(e) => return Resolution::NotFound(e.to_string()),
    };
    found.sort_by_key(|c| c.con_id);
    found.dedup_by_key(|c| c.con_id);
    if let Some(ex) = &row.exchange {
        found.retain(|c| &c.primary_exchange == ex || &c.exchange == ex);
    }
    let what = row.symbol.clone().or(row.con_id.map(|c| c.to_string())).unwrap_or_default();
    match found.len() {
        0 => Resolution::NotFound(format!("IBKR has no contract for {what}")),
        1 => {
            let c = found.remove(0);
            match to_mic(&c.primary_exchange) {
                Some(_) => Resolution::Found(c),
                None => Resolution::NotFound(format!(
                    "{what} trades on '{}', an exchange the OMS has no MIC for",
                    c.primary_exchange
                )),
            }
        }
        _ => Resolution::Ambiguous(found),
    }
}

/// The futures root a FUT contract belongs to (a `future_root` row): its code
/// is the derivative's underlying, its asset class the instrument's.
#[derive(Debug, Clone, PartialEq)]
pub struct RootSpec {
    pub code: String,
    pub asset_class: String,
}

/// The OMS's record of a resolved contract, routed by conId.
/// STK: `symbol@MIC`, `EQUITY` / `SPOT`. FUT: `localSymbol@MIC`, `FUTURE`, with the
/// multiplier and last-trade date IBKR gave. OPT: `EQUITY` / `OPTION`.
/// `root` is required for FUT and ignored otherwise.
pub fn to_broker_instrument(c: &TwsContract, root: Option<&RootSpec>) -> Result<BrokerInstrument, String> {
    // Futures have no primary exchange; they trade where they are routed.
    let mic_of = if c.sec_type == SecType::Fut || c.primary_exchange.is_empty() { &c.exchange } else { &c.primary_exchange };
    let mic = to_mic(mic_of)
        .ok_or_else(|| format!("{} trades on '{}', an exchange the OMS has no MIC for", c.symbol, mic_of))?;
    if c.min_tick <= 0.0 {
        return Err(format!("IBKR gave no minimum tick for {} (conId {})", c.symbol, c.con_id));
    }
    let lot_size = c.lot_size.filter(|l| *l > 0.0);
    let mut definition = InstrumentDef {
        symbol: c.symbol.clone(),
        venue: mic.to_string(),
        currency: c.currency.clone(),
        asset_class: "EQUITY".to_string(),
        instrument_class: "SPOT".to_string(),
        name: Some(c.name.clone()).filter(|n| !n.is_empty()),
        price_precision: decimals(c.min_tick),
        price_increment: c.min_tick,
        size_increment: lot_size.unwrap_or(1.0),
        lot_size,
        contract_size: 1.0,
        native_id: Some(c.con_id.to_string()),
        provider_exchange: Some(mic.to_string()),
        derivative: None,
    };
    let (broker_symbol, broker_exchange) = match c.sec_type {
        SecType::Stk => (c.symbol.clone(), c.primary_exchange.clone()),
        SecType::Fut | SecType::Opt => {
            if c.local_symbol.is_empty() {
                return Err(format!("IBKR gave no local symbol for {} (conId {})", c.symbol, c.con_id));
            }
            if c.multiplier <= 0.0 {
                return Err(format!("IBKR gave no multiplier for {} (conId {})", c.local_symbol, c.con_id));
            }
            let Some(expiry) = c.last_trade else {
                return Err(format!("IBKR gave no last trade date for {} (conId {})", c.local_symbol, c.con_id));
            };
            definition.symbol = c.local_symbol.clone();
            definition.contract_size = c.multiplier;
            if c.sec_type == SecType::Fut {
                let root = root.ok_or_else(|| {
                    format!("{} (conId {}) is a future but its root is not configured", c.local_symbol, c.con_id)
                })?;
                definition.asset_class = root.asset_class.clone();
                definition.instrument_class = "FUTURE".to_string();
                definition.derivative = Some(DerivativeDef {
                    underlying_symbol: root.code.clone(),
                    option_kind: None,
                    strike_price: None,
                    expiry_date: Some(expiry),
                    expires_at: c.last_trade_at,
                    activation_date: None,
                });
            } else {
                let kind = match c.right.as_deref() {
                    Some("C") | Some("CALL") => OptionKind::Call,
                    Some("P") | Some("PUT") => OptionKind::Put,
                    other => {
                        return Err(format!("IBKR gave option right {other:?} for conId {}", c.con_id));
                    }
                };
                let strike = c
                    .strike
                    .filter(|s| *s > 0.0)
                    .ok_or_else(|| format!("IBKR gave no strike for {} (conId {})", c.local_symbol, c.con_id))?;
                definition.instrument_class = "OPTION".to_string();
                definition.derivative = Some(DerivativeDef {
                    underlying_symbol: c.symbol.clone(),
                    option_kind: Some(kind),
                    strike_price: Some(strike),
                    expiry_date: Some(expiry),
                    expires_at: c.last_trade_at,
                    activation_date: None,
                });
            }
            (c.local_symbol.clone(), c.exchange.clone())
        }
        SecType::Bag => return Err(format!("conId {} is a combo, which the OMS does not import", c.con_id)),
    };
    Ok(BrokerInstrument {
        broker_symbol,
        broker_exchange: Some(broker_exchange),
        is_tradeable: true,
        min_quantity: None,
        max_quantity: None,
        min_notional: None,
        max_notional: None,
        definition,
    })
}

/// Decimal places a tick size needs: 0.01 -> 2, 0.0005 -> 4.
fn decimals(tick: f64) -> i32 {
    (0..=8).find(|p| ((tick * 10f64.powi(*p)).round() - tick * 10f64.powi(*p)).abs() < 1e-9).unwrap_or(8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ibkr_tws::client::FakeTws;

    fn contract(con_id: i64, primary: &str, currency: &str) -> TwsContract {
        TwsContract {
            con_id,
            symbol: "SAP".into(),
            exchange: "SMART".into(),
            primary_exchange: primary.into(),
            currency: currency.into(),
            min_tick: 0.01,
            lot_size: None,
            name: "SAP SE".into(),
            isin: Some("DE0007164600".into()),
            sec_type: SecType::Stk,
            local_symbol: String::new(),
            multiplier: 1.0,
            last_trade: None,
            last_trade_at: None,
            strike: None,
            right: None,
        }
    }

    fn row(symbol: &str, exchange: Option<&str>, currency: Option<&str>) -> CsvRow {
        CsvRow {
            line: 2,
            symbol: Some(symbol.into()),
            exchange: exchange.map(Into::into),
            currency: currency.map(Into::into),
            con_id: None,
        }
    }

    fn fake_with_sap() -> FakeTws {
        let fake = FakeTws::new();
        let both = vec![contract(1, "NYSE", "USD"), contract(2, "IBIS", "EUR")];
        fake.details.lock().unwrap().insert(
            ContractQuery { symbol: Some("SAP".into()), ..Default::default() },
            both.clone(),
        );
        fake.details.lock().unwrap().insert(
            ContractQuery { symbol: Some("SAP".into()), currency: Some("EUR".into()), ..Default::default() },
            both[1..].to_vec(),
        );
        fake
    }

    fn es_contract() -> TwsContract {
        TwsContract {
            con_id: 495512563,
            symbol: "ES".into(),
            exchange: "CME".into(),
            primary_exchange: String::new(),
            currency: "USD".into(),
            min_tick: 0.25,
            lot_size: None,
            name: "E-mini S&P 500".into(),
            isin: None,
            sec_type: SecType::Fut,
            local_symbol: "ESZ6".into(),
            multiplier: 50.0,
            last_trade: chrono::NaiveDate::from_ymd_opt(2026, 12, 18),
            last_trade_at: None,
            strike: None,
            right: None,
        }
    }

    fn es_root() -> RootSpec {
        RootSpec { code: "ES".into(), asset_class: "INDEX".into() }
    }

    #[test]
    fn fut_maps_to_future_with_multiplier_and_expiry() {
        let b = to_broker_instrument(&es_contract(), Some(&es_root())).unwrap();
        let d = &b.definition;
        assert_eq!(
            (d.symbol.as_str(), d.venue.as_str(), d.asset_class.as_str(), d.instrument_class.as_str()),
            ("ESZ6", "XCME", "INDEX", "FUTURE")
        );
        assert_eq!(d.contract_size, 50.0);
        assert_eq!(d.native_id.as_deref(), Some("495512563"));
        assert_eq!(b.broker_symbol, "ESZ6");
        assert_eq!(b.broker_exchange.as_deref(), Some("CME"));
        let dv = d.derivative.as_ref().unwrap();
        assert_eq!(dv.underlying_symbol, "ES");
        assert_eq!(dv.expiry_date, chrono::NaiveDate::from_ymd_opt(2026, 12, 18));
        assert_eq!(dv.expires_at, None);

        let mut c = es_contract();
        let at = chrono::DateTime::parse_from_rfc3339("2026-12-18T14:30:00Z").unwrap().with_timezone(&chrono::Utc);
        c.last_trade_at = Some(at);
        let b = to_broker_instrument(&c, Some(&es_root())).unwrap();
        assert_eq!(b.definition.derivative.unwrap().expires_at, Some(at));
    }

    #[test]
    fn fut_without_root_is_refused() {
        let err = to_broker_instrument(&es_contract(), None).err().unwrap();
        assert!(err.contains("495512563"), "{err}");
    }

    #[test]
    fn fut_without_last_trade_or_multiplier_is_refused() {
        let mut c = es_contract();
        c.last_trade = None;
        assert!(to_broker_instrument(&c, Some(&es_root())).err().unwrap().contains("495512563"));
        let mut c = es_contract();
        c.multiplier = 0.0;
        assert!(to_broker_instrument(&c, Some(&es_root())).err().unwrap().contains("495512563"));
    }

    #[test]
    fn opt_maps_to_option_with_strike_and_kind() {
        let c = TwsContract {
            con_id: 700,
            symbol: "AAPL".into(),
            exchange: "SMART".into(),
            primary_exchange: "NASDAQ".into(),
            currency: "USD".into(),
            min_tick: 0.01,
            lot_size: None,
            name: "APPLE".into(),
            isin: None,
            sec_type: SecType::Opt,
            local_symbol: "AAPL  261218C00200000".into(),
            multiplier: 100.0,
            last_trade: chrono::NaiveDate::from_ymd_opt(2026, 12, 18),
            last_trade_at: None,
            strike: Some(200.0),
            right: Some("C".into()),
        };
        let b = to_broker_instrument(&c, None).unwrap();
        let d = &b.definition;
        assert_eq!((d.asset_class.as_str(), d.instrument_class.as_str()), ("EQUITY", "OPTION"));
        assert_eq!(d.symbol, "AAPL  261218C00200000");
        assert_eq!(d.contract_size, 100.0);
        let dv = d.derivative.as_ref().unwrap();
        assert_eq!(dv.underlying_symbol, "AAPL");
        assert_eq!(dv.strike_price, Some(200.0));
        assert!(matches!(dv.option_kind, Some(dataprovider::OptionKind::Call)));
    }

    #[test]
    fn header_in_any_order_and_mic_normalised() {
        let rows = parse_csv("currency,Symbol,exchange\nEUR,sap,XETR\n\nUSD,aapl,\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].line, 2);
        assert_eq!(rows[0].symbol.as_deref(), Some("SAP"));
        assert_eq!(rows[0].exchange.as_deref(), Some("IBIS"));
        assert_eq!(rows[1].line, 4);
        assert_eq!(rows[1].exchange, None);
    }

    #[test]
    fn conid_only_rows_are_accepted() {
        let rows = parse_csv("conid\n265598\n").unwrap();
        assert_eq!(rows[0].con_id, Some(265598));
        assert_eq!(rows[0].symbol, None);
    }

    #[test]
    fn row_without_symbol_or_conid_names_its_line() {
        let err = parse_csv("symbol,exchange\nAAPL,\n,NYSE\n").unwrap_err();
        assert!(err.starts_with("line 3"), "{err}");
    }

    #[test]
    fn header_without_symbol_or_conid_is_refused() {
        assert!(parse_csv("exchange,currency\nNYSE,USD\n").is_err());
        assert!(parse_csv("").is_err());
    }

    #[tokio::test]
    async fn ambiguous_symbol_lists_every_candidate_and_saves_nothing() {
        let fake = fake_with_sap();
        match resolve(&fake, &row("SAP", None, None)).await {
            Resolution::Ambiguous(c) => assert_eq!(c.iter().map(|c| c.con_id).collect::<Vec<_>>(), vec![1, 2]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exchange_narrows_to_one() {
        let fake = fake_with_sap();
        let r = resolve(&fake, &row("SAP", Some("IBIS"), Some("EUR"))).await;
        assert_eq!(r, Resolution::Found(contract(2, "IBIS", "EUR")));
        // Exchange alone, over the unfiltered candidate list, also narrows.
        let r = resolve(&fake, &row("SAP", Some("NYSE"), None)).await;
        assert_eq!(r, Resolution::Found(contract(1, "NYSE", "USD")));
    }

    #[tokio::test]
    async fn conid_resolves_exactly() {
        let fake = FakeTws::new();
        fake.details.lock().unwrap().insert(
            ContractQuery { con_id: Some(2), ..Default::default() },
            vec![contract(2, "IBIS", "EUR")],
        );
        let r = CsvRow { line: 2, symbol: None, exchange: None, currency: None, con_id: Some(2) };
        assert!(matches!(resolve(&fake, &r).await, Resolution::Found(c) if c.con_id == 2));
        let missing = CsvRow { con_id: Some(9), ..r };
        assert!(matches!(resolve(&fake, &missing).await, Resolution::NotFound(_)));
    }

    #[tokio::test]
    async fn unknown_primary_exchange_is_reported_not_guessed() {
        let fake = FakeTws::new();
        fake.details.lock().unwrap().insert(
            ContractQuery { symbol: Some("ZZZ".into()), ..Default::default() },
            vec![contract(7, "MOON", "USD")],
        );
        match resolve(&fake, &row("ZZZ", None, None)).await {
            Resolution::NotFound(m) => assert!(m.contains("MOON"), "{m}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(to_broker_instrument(&contract(7, "MOON", "USD"), None).is_err());
    }

    #[test]
    fn broker_instrument_is_keyed_by_con_id_on_the_mic() {
        let bi = to_broker_instrument(&contract(2, "IBIS", "EUR"), None).unwrap();
        let d = &bi.definition;
        assert_eq!((d.symbol.as_str(), d.venue.as_str()), ("SAP", "XETR"));
        assert_eq!(d.native_id.as_deref(), Some("2"));
        assert_eq!((d.asset_class.as_str(), d.instrument_class.as_str()), ("EQUITY", "SPOT"));
        assert_eq!((d.price_precision, d.size_increment), (2, 1.0));
        assert_eq!(bi.broker_exchange.as_deref(), Some("IBIS"));
    }

    #[test]
    fn tick_decimals() {
        assert_eq!(decimals(0.01), 2);
        assert_eq!(decimals(0.0005), 4);
        assert_eq!(decimals(1.0), 0);
    }
}
