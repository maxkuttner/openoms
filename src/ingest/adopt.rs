//! Adopting a venue order the OMS did not place: its instrument (added from
//! the broker's catalog when missing), its owner, and its submit + route
//! commands.

use sqlx::{PgConnection, PgPool};
use tracing::info;
use uuid::Uuid;

use gateway::adapter::{BrokerInstrument, InstrumentProvider};
use gateway::venue_event::{VenueEvent, VenueFill, VenueOrderUpdate};

use super::apply::IngestError;
use crate::domain::orders::commands::{OrderCommand, RouteOrder, SubmitOrder};
use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};

/// Submit and route an order the OMS did not place, sized by what the venue
/// has reported so far: the update's quantity, or the fill's.
pub(super) fn commands(
    order_id: Uuid,
    portfolio_id: Uuid,
    account: Uuid,
    instrument: i64,
    venue: &str,
    event: &VenueEvent,
) -> Vec<OrderCommand> {
    let (client, venue_order_id, buy, order_type, limit, qty) = match event {
        VenueEvent::Fill(VenueFill { client_order_id, venue_order_id, buy, price, qty, .. }) => {
            (client_order_id, venue_order_id, *buy, OrderType::Limit, Some(*price), *qty)
        }
        VenueEvent::Order(VenueOrderUpdate { client_order_id, venue_order_id, buy, order_type, limit_px, qty, .. }) => {
            let ty = if order_type.eq_ignore_ascii_case("market") { OrderType::Market } else { OrderType::Limit };
            (client_order_id, venue_order_id, *buy, ty, if ty == OrderType::Market { None } else { *limit_px }, *qty)
        }
        _ => unreachable!("only orders and fills are adopted"),
    };
    vec![
        OrderCommand::SubmitOrder(SubmitOrder {
            order_id: order_id.to_string(),
            client_order_id: client.clone().unwrap_or_else(|| venue_order_id.clone()),
            portfolio_id: portfolio_id.to_string(),
            account_id: account.to_string(),
            instrument_id: instrument.to_string(),
            side: if buy { OrderSide::Buy } else { OrderSide::Sell },
            order_type,
            time_in_force: TimeInForce::Gtc,
            limit_price: limit,
            quantity: qty,
        }),
        OrderCommand::RouteOrder(RouteOrder {
            order_id: order_id.to_string(),
            venue: venue.into(),
            external_order_id: venue_order_id.clone(),
        }),
    ]
}

/// Who an adopted order belongs to: the book's strategy, else someone who may
/// trade the book, else the first administrator.
pub(super) async fn owner(conn: &mut PgConnection, portfolio_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM ( \
            SELECT principal_id AS id, 1 AS rank, now() AS at FROM strategy WHERE portfolio_id = $1 \
            UNION ALL SELECT principal_id, 2, created_at FROM principal_portfolio_grant \
                WHERE portfolio_id = $1 AND can_trade \
            UNION ALL SELECT principal_id, 3, granted_at FROM principal_admin \
         ) c ORDER BY rank, at LIMIT 1",
    )
    .bind(portfolio_id)
    .fetch_optional(conn)
    .await
}

/// One instrument as a catalog provider, to add a symbol the catalog lacks.
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

async fn instrument_id(pool: &PgPool, broker: &str, symbol: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT instrument_id FROM broker_instrument WHERE broker_symbol = $1 AND broker_code = $2")
        .bind(symbol)
        .bind(broker)
        .fetch_optional(pool)
        .await
}

/// The catalog's instrument for the broker's `symbol`, added from the broker's
/// own catalog when missing. `None` when the broker does not list it (or there
/// is no catalog to ask).
pub async fn ensure_instrument(
    pool: &PgPool,
    broker: &str,
    provider: Option<&dyn InstrumentProvider>,
    symbol: &str,
) -> Result<Option<i64>, IngestError> {
    if let Some(id) = instrument_id(pool, broker, symbol).await? {
        return Ok(Some(id));
    }
    let Some(provider) = provider else { return Ok(None) };
    let listed = provider.list_instruments(&[]).await.map_err(|e| IngestError::Other(e.to_string()))?;
    let Some(found) = listed.into_iter().find(|i| i.broker_symbol == symbol) else { return Ok(None) };
    let Some(b) = crate::setup::brokers::Broker::by_code(broker) else { return Ok(None) };
    let opts = crate::setup::brokers::LoadOptions { no_enrich: true, allow_skips: true, underlyings: Vec::new() };
    crate::setup::brokers::load_universe(pool, b, &OneInstrument(std::sync::Mutex::new(Some(found))), &opts, &Quiet)
        .await
        .map_err(|e| IngestError::Other(e.to_string()))?;
    info!(broker, symbol, "ingest: added an instrument the catalog did not have");
    Ok(instrument_id(pool, broker, symbol).await?)
}
