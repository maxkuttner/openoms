//! A followed account's fill, booked as an external order of the strategy's
//! portfolio: one order per venue trade, so a fill that arrives twice (live,
//! then again from the trade history) finds its order and books nothing.

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use gateway::adapter::{BrokerInstrument, InstrumentProvider};
use gateway::connectors::binance::adapter::BinanceAdapter;
use gateway::contract::{ExecEvent, ExecKind, ExecSource, ExternalFill};

use crate::app_state::AppState;
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{OrderCommand, RouteOrder, SubmitOrder};
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};
use crate::event_bus::publish_appended;
use crate::event_store::OrderEventStore;
use crate::handlers::domain_event_to_new_event;
use crate::order_entry::{insert_order_state, Origin};
use crate::strategies::Strategy;

/// Namespace of external order ids (a fixed v4, so ids never collide with
/// order ids the OMS generates).
const COLO_NAMESPACE: Uuid = Uuid::from_u128(0x6f1c_3a2e_9b84_4d17_a5e0_c2b9_7d41_0f63);

/// The OMS order id of one venue trade on a strategy's account. Binance
/// numbers trades per pair, so the pair is part of the identity.
pub fn external_order_id(strategy_code: &str, symbol: &str, trade_id: &str) -> Uuid {
    Uuid::new_v5(&COLO_NAMESPACE, format!("{strategy_code}:{symbol}:{trade_id}").as_bytes())
}

/// What is already booked for one trade.
#[derive(Debug, PartialEq)]
pub enum Existing {
    Missing,
    /// The order exists but its fill never reached the engine (a crash or a
    /// dropped event between the two steps): send the fill again.
    NeedsFill,
    Booked,
}

/// From the trade's order's cumulative quantity, if the order exists.
pub fn existing_order(cum_qty: Option<f64>) -> Existing {
    match cum_qty {
        None => Existing::Missing,
        Some(q) if q <= 0.0 => Existing::NeedsFill,
        Some(_) => Existing::Booked,
    }
}

/// The strategy's tag in a client order id: everything before its second `-`
/// (`mm1-q-0001` → `mm1-q`). `None` when there is no second `-`.
pub fn tag_of(client_order_id: &str) -> Option<&str> {
    let first = client_order_id.find('-')?;
    let second = client_order_id[first + 1..].find('-')? + first + 1;
    Some(&client_order_id[..second])
}

#[derive(Debug, PartialEq)]
pub enum Booked {
    New,
    AlreadyBooked,
}

/// One instrument as a catalog provider, to add a pair the catalog lacks.
struct OneInstrument(std::sync::Mutex<Option<BrokerInstrument>>);

#[async_trait::async_trait]
impl InstrumentProvider for OneInstrument {
    async fn list_instruments(&self, _: &[String]) -> Result<Vec<BrokerInstrument>, gateway::adapter::BrokerError> {
        Ok(self.0.lock().unwrap().take().into_iter().collect())
    }
}

struct Quiet;

impl crate::setup::brokers::LoadProgress for Quiet {
    fn progress(&self, _: i64, _: i64) {}
    fn log(&self, _: &str, _: &str) {}
}

async fn instrument_id(pool: &PgPool, symbol: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT instrument_id FROM broker_instrument WHERE broker_symbol = $1 AND broker_code = 'BINANCE'")
        .bind(symbol)
        .fetch_optional(pool)
        .await
}

/// The catalog's instrument for `symbol`, added from Binance's exchange
/// information when the catalog does not have it yet.
async fn ensure_instrument(pool: &PgPool, adapter: &BinanceAdapter, symbol: &str) -> Result<i64, String> {
    if let Some(id) = instrument_id(pool, symbol).await.map_err(|e| e.to_string())? {
        return Ok(id);
    }
    let found = adapter.symbol_instrument(symbol).await.map_err(|e| e.to_string())?;
    let found = found.ok_or_else(|| format!("Binance does not list {symbol}"))?;
    let broker = crate::setup::brokers::Broker::by_code("BINANCE").ok_or("no Binance connector")?;
    let opts = crate::setup::brokers::LoadOptions { no_enrich: true, allow_skips: true, underlyings: Vec::new() };
    crate::setup::brokers::load_universe(pool, broker, &OneInstrument(std::sync::Mutex::new(Some(found))), &opts, &Quiet)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(symbol, "co-located: added a pair the catalog did not have");
    instrument_id(pool, symbol).await.map_err(|e| e.to_string())?.ok_or_else(|| format!("{symbol} could not be added to the catalog"))
}

/// Book one fill of a followed account: create its external order (submitted
/// and routed), then hand the fill to the order engine, which books it and
/// moves the position like any other fill.
pub async fn book_fill(state: &AppState, strategy: &Strategy, adapter: &BinanceAdapter, fill: &ExternalFill) -> Result<Booked, String> {
    let pool = state.pool();
    let order_id = external_order_id(&strategy.code, &fill.symbol, &fill.trade_id);
    let cum: Option<f64> = sqlx::query_scalar("SELECT cum_qty::double precision FROM order_state WHERE order_id = $1")
        .bind(order_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?;
    match existing_order(cum) {
        Existing::Booked => return Ok(Booked::AlreadyBooked),
        Existing::NeedsFill => {
            send_fill(state, order_id, fill).await;
            return Ok(Booked::New);
        }
        Existing::Missing => {}
    }
    let instrument = ensure_instrument(pool, adapter, &fill.symbol).await?;
    let account = *strategy.connections.get("BINANCE").ok_or("the strategy has no BINANCE account")?;
    let connection: String = sqlx::query_scalar("SELECT broker_connection_code FROM account WHERE id = $1")
        .bind(account)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;

    let actor = format!("colo:{}", strategy.code);
    let mut aggregate = OrderAggregate::empty();
    let mut events = Vec::new();
    for command in [
        OrderCommand::SubmitOrder(SubmitOrder {
            order_id: order_id.to_string(),
            client_order_id: fill.client_order_id.clone(),
            portfolio_id: strategy.portfolio_id.to_string(),
            account_id: account.to_string(),
            instrument_id: instrument.to_string(),
            side: if fill.buy { OrderSide::Buy } else { OrderSide::Sell },
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Gtc,
            limit_price: Some(fill.price),
            quantity: fill.qty,
        }),
        OrderCommand::RouteOrder(RouteOrder {
            order_id: order_id.to_string(),
            venue: "BINANCE".into(),
            external_order_id: fill.venue_order_id.clone(),
        }),
    ] {
        let metadata = EventMetadata { event_id: Uuid::new_v4().to_string(), timestamp: Utc::now(), actor: actor.clone() };
        let decided = aggregate.decide(command, metadata).map_err(|e| format!("{e:?}"))?;
        for event in &decided {
            aggregate.apply(event).map_err(|e| format!("{e:?}"))?;
        }
        events.extend(decided);
    }
    let routed = aggregate.state.clone().ok_or("no order state")?;
    let new_events = events.iter().map(domain_event_to_new_event).collect::<Result<Vec<_>, _>>().map_err(|e| e.message)?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    insert_order_state(&mut tx, order_id, &routed, strategy.principal_id, &connection, &Origin::External, &None)
        .await
        .map_err(|e| e.message)?;
    sqlx::query("UPDATE order_state SET external_order_id = $2, venue_trade_id = $3 WHERE order_id = $1")
        .bind(order_id)
        .bind(&fill.venue_order_id)
        .bind(fill.trade_id.parse::<i64>().ok())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let appended = OrderEventStore::new(pool.clone())
        .append_events_in_tx(&mut tx, order_id, 0, &new_events)
        .await
        .map_err(|e| format!("{e:?}"))?;
    match tx.commit().await {
        Ok(()) => {}
        // A concurrent booking of the same trade won the insert.
        Err(e) if e.to_string().contains("duplicate key") => return Ok(Booked::AlreadyBooked),
        Err(e) => return Err(e.to_string()),
    }
    publish_appended(state.event_bus(), order_id, &new_events, &appended);
    send_fill(state, order_id, fill).await;
    Ok(Booked::New)
}

/// Hand a trade's fill to the order engine, which books it and the position.
async fn send_fill(state: &AppState, order_id: Uuid, fill: &ExternalFill) {
    state
        .engine()
        .exec(ExecEvent {
            order_id,
            venue: "BINANCE".into(),
            actor: "colo",
            source: ExecSource::Session,
            kind: ExecKind::Fill { execution_id: fill.trade_id.clone(), qty: fill.qty, price: fill.price },
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the shared dev DB and Binance testnet's public exchange info:
    /// a fill on a pair the catalog lacks adds the pair, then books an
    /// external order once.
    #[tokio::test]
    #[ignore]
    async fn an_unknown_pair_is_added_before_booking() {
        use crate::strategies::test_support as t;
        let pool = t::dev_pool().await;
        let code = t::seed_strategy(&pool).await;
        let state = t::state(pool.clone());
        let (account,): (Uuid,) = sqlx::query_as("SELECT id FROM account a JOIN broker_connection bc ON bc.code = a.broker_connection_code WHERE bc.broker_code = 'BINANCE' LIMIT 1")
            .fetch_one(&pool).await.unwrap();
        sqlx::query("INSERT INTO strategy_connection (strategy_code, broker_code, account_id) VALUES ($1, 'BINANCE', $2)")
            .bind(&code).bind(account).execute(&pool).await.unwrap();
        state.strategies().reload(&pool).await.unwrap();
        let strategy = state.strategies().by_code(&code).unwrap();
        // Any testnet Ed25519 key: only public exchange info is read.
        let pem = std::env::var("BINANCE_TEST_PRIVATE_KEY").expect("BINANCE_TEST_PRIVATE_KEY");
        let adapter = BinanceAdapter::new("unused".into(), &pem, "PAPER").expect("a testnet adapter");
        let fill = ExternalFill { trade_id: "1".into(), venue_order_id: "9".into(), client_order_id: "mm1-q-1".into(),
            symbol: "ETHBTC".into(), buy: true, qty: 0.001, price: 0.03 };
        assert_eq!(book_fill(&state, &strategy, &adapter, &fill).await.unwrap(), Booked::New);
        assert_eq!(book_fill(&state, &strategy, &adapter, &fill).await.unwrap(), Booked::AlreadyBooked);
        t::drop_orders(&pool, &code).await;
        t::drop_strategy(&pool, &code).await;
    }

    #[test]
    fn a_repeated_fill_books_once() {
        assert_eq!(external_order_id("mm1", "BTCUSDT", "77"), external_order_id("mm1", "BTCUSDT", "77"));
        assert_ne!(external_order_id("mm1", "BTCUSDT", "77"), external_order_id("mm1", "BTCUSDT", "78"));
        assert_ne!(external_order_id("mm1", "BTCUSDT", "77"), external_order_id("mm2", "BTCUSDT", "77"));
    }

    /// Binance numbers trades per pair: the same trade id on two pairs is two fills.
    #[test]
    fn the_same_trade_id_on_two_pairs_is_two_fills() {
        assert_ne!(external_order_id("mm1", "BTCUSDT", "77"), external_order_id("mm1", "ETHBTC", "77"));
    }

    /// An order created but whose fill never reached the engine is filled again.
    #[test]
    fn an_order_without_its_fill_gets_the_fill_again() {
        assert_eq!(existing_order(Some(0.0)), Existing::NeedsFill);
        assert_eq!(existing_order(Some(0.5)), Existing::Booked);
        assert_eq!(existing_order(None), Existing::Missing);
    }

    #[test]
    fn the_tag_is_the_client_id_up_to_its_second_dash() {
        assert_eq!(tag_of("mm1-q-0001"), Some("mm1-q"));
        assert_eq!(tag_of("mm1-h-0002-x"), Some("mm1-h"));
        assert_eq!(tag_of("mm1-q"), None);
        assert_eq!(tag_of("x"), None);
        assert_eq!(tag_of(""), None);
    }
}
