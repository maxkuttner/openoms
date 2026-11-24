//! Adopting a venue order the OMS did not place: its instrument (added from
//! the broker's catalog when missing), its owner, and its observe command.

use sqlx::{PgConnection, PgPool};
use tracing::info;
use uuid::Uuid;

use gateway::adapter::{BrokerInstrument, InstrumentProvider};
use gateway::venue_event::{VenueEvent, VenueFill, VenueOrderStatus};

use super::apply::IngestError;
use crate::domain::orders::commands::{ObserveOrder, OrderCommand};
use crate::domain::orders::state::{OrderSide, OrderStatus, OrderType, TimeInForce};

/// Observe an order the OMS did not place, as the venue reports it so far:
/// an update's status, quantity and cum, or a first-seen fill's (partially
/// filled, sized by the fill). Fills book only from fill events.
pub(super) fn observe(
    order_id: Uuid,
    portfolio_id: Uuid,
    account: Uuid,
    instrument: i64,
    venue: &str,
    event: &VenueEvent,
) -> OrderCommand {
    let (client, venue_order_id, buy, order_type, limit, qty, status, venue_cum, created) = match event {
        VenueEvent::Fill(VenueFill { client_order_id, venue_order_id, buy, price, qty, .. }) => (
            client_order_id,
            venue_order_id,
            *buy,
            OrderType::Limit,
            Some(*price),
            *qty,
            OrderStatus::PartiallyFilled,
            *qty,
            None,
        ),
        VenueEvent::Order(o) => {
            let ty = if o.order_type.eq_ignore_ascii_case("market") { OrderType::Market } else { OrderType::Limit };
            let status = match &o.status {
                VenueOrderStatus::Working if o.cum_qty > 0.0 => OrderStatus::PartiallyFilled,
                VenueOrderStatus::Working => OrderStatus::Routed,
                VenueOrderStatus::Filled => OrderStatus::Filled,
                VenueOrderStatus::Canceled => OrderStatus::Canceled,
                VenueOrderStatus::Rejected { .. } => OrderStatus::Rejected,
                VenueOrderStatus::Expired => OrderStatus::Expired,
            };
            (
                &o.client_order_id,
                &o.venue_order_id,
                o.buy,
                ty,
                if ty == OrderType::Market { None } else { o.limit_px },
                o.qty,
                status,
                o.cum_qty.clamp(0.0, o.qty),
                o.created_at,
            )
        }
        _ => unreachable!("only orders and fills are adopted"),
    };
    OrderCommand::ObserveOrder(ObserveOrder {
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
        venue: venue.into(),
        external_order_id: venue_order_id.clone(),
        observed_status: status,
        venue_cum_qty: venue_cum,
        venue_created_at: created,
    })
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
    let opts = crate::setup::brokers::LoadOptions { allow_skips: true, underlyings: Vec::new() };
    crate::setup::brokers::load_universe(pool, b, &OneInstrument(std::sync::Mutex::new(Some(found))), &opts, &Quiet)
        .await
        .map_err(|e| IngestError::Other(e.to_string()))?;
    info!(broker, symbol, "ingest: added an instrument the catalog did not have");
    Ok(instrument_id(pool, broker, symbol).await?)
}
