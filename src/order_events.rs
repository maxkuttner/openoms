//! The per-order audit trail: the event stream behind an order, rendered for reading.
//!
//! The event store is the order's system of record — every transition is already
//! there, append-only and immutable (`db/migrations/ods/oms/0006_*`). Nothing here
//! adds state; this module only projects that stream into something a trader or a
//! compliance reader can scan, and serves it over HTTP.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{query_scalar, PgPool};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload};
use crate::event_store::{OrderEventRecord, OrderEventStore};
use crate::handlers::{require_order_grant, ApiError, OrderPermission};

/// One entry in an order's audit trail.
///
/// `occurred_at` is when the domain decided the event; `recorded_at` is when the
/// store appended it. They differ for anything that arrives from a broker stream,
/// and an auditor wants both.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct OrderEventView {
    pub version: i64,
    pub event_id: String,
    pub event_type: String,
    pub actor: String,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub status_after: Option<String>,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub schema_version: i32,
    pub summary: String,
    /// The event body exactly as stored — the forensic record, not a rendering of it.
    pub payload: Value,
}

/// Project one stored row into a timeline entry.
///
/// A row whose payload no longer parses as a domain event is still shown: the
/// envelope and the raw JSON are all recoverable without it, and dropping the row
/// (or failing the whole request) would be the one thing an append-only audit log
/// must never do.
pub fn view(record: &OrderEventRecord) -> OrderEventView {
    let event = serde_json::from_value::<OrderDomainEvent>(record.payload.clone()).ok();

    OrderEventView {
        version: record.version,
        event_id: record.event_id.clone(),
        event_type: record.event_type.clone(),
        actor: record.actor.clone(),
        occurred_at: event.as_ref().map_or(record.created_at, |e| e.timestamp),
        recorded_at: record.created_at,
        status_after: event.as_ref().map(|e| e.status_after.as_str().to_string()),
        correlation_id: record.correlation_id.clone(),
        causation_id: record.causation_id.clone(),
        schema_version: record.schema_version,
        summary: match &event {
            Some(e) => summarize(e),
            None => format!(
                "{} (payload not readable at schema version {})",
                record.event_type, record.schema_version
            ),
        },
        // The stored row wraps the event body in a `payload` key; anything we could
        // not parse is handed back whole rather than guessing at its shape.
        payload: match &event {
            Some(_) => record.payload.get("payload").cloned().unwrap_or(Value::Null),
            None => record.payload.clone(),
        },
    }
}

/// One line describing what an event did, rendered server-side so the cockpit and
/// the Python CLI word it identically.
pub fn summarize(event: &OrderDomainEvent) -> String {
    match &event.payload {
        OrderEventPayload::OrderSubmitted {
            instrument_id,
            side,
            order_type,
            time_in_force,
            limit_price,
            quantity,
            ..
        } => {
            let price = match limit_price {
                Some(px) => format!("{} {px}", order_type.as_str()),
                None => order_type.as_str().to_string(),
            };
            format!(
                "submitted {} {quantity} {instrument_id} {price} ({})",
                side.as_str(),
                time_in_force.as_str()
            )
        }
        OrderEventPayload::OrderRejected { reason } => format!("rejected: {reason}"),
        OrderEventPayload::OrderRouted {
            venue,
            external_order_id,
        } => format!("routed to {venue} as {external_order_id}"),
        OrderEventPayload::OrderAmended {
            previous_limit_price,
            new_limit_price,
            previous_quantity,
            new_quantity,
        } => {
            let mut changes = Vec::new();
            if previous_quantity != new_quantity {
                changes.push(format!("quantity {previous_quantity} \u{2192} {new_quantity}"));
            }
            if previous_limit_price != new_limit_price {
                changes.push(format!(
                    "limit {} \u{2192} {}",
                    optional_price(*previous_limit_price),
                    optional_price(*new_limit_price)
                ));
            }
            if changes.is_empty() {
                "amended (no change)".to_string()
            } else {
                format!("amended {}", changes.join(", "))
            }
        }
        OrderEventPayload::OrderCanceled { reason } => with_reason("canceled", reason.as_deref()),
        OrderEventPayload::CancelRejected { reason } => format!("cancel rejected: {reason}"),
        OrderEventPayload::OrderExpired { reason } => with_reason("expired", reason.as_deref()),
        OrderEventPayload::OrderSuspended {
            reason,
            resume_to_status,
        } => format!(
            "{} (resumes to {})",
            with_reason("suspended", reason.as_deref()),
            resume_to_status.as_str()
        ),
        OrderEventPayload::OrderReleased { resumed_to_status } => {
            format!("released to {}", resumed_to_status.as_str())
        }
        OrderEventPayload::OrderPartiallyFilled {
            fill_qty,
            fill_price,
            cum_qty,
            leaves_qty,
            avg_px,
            venue,
            ..
        } => format!(
            "partially filled {fill_qty} @ {fill_price} on {venue} \
             ({cum_qty} cumulative, {leaves_qty} working, avg {avg_px})"
        ),
        OrderEventPayload::OrderFilled {
            fill_qty,
            fill_price,
            cum_qty,
            avg_px,
            venue,
            ..
        } => format!(
            "filled {fill_qty} @ {fill_price} on {venue} ({cum_qty} cumulative, avg {avg_px})"
        ),
    }
}

/// `"canceled"` on its own, `"canceled: <reason>"` when the broker gave one.
fn with_reason(verb: &str, reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("{verb}: {reason}"),
        None => verb.to_string(),
    }
}

/// A limit price that an amendment may have set or cleared.
fn optional_price(price: Option<f64>) -> String {
    match price {
        Some(px) => px.to_string(),
        None => "none".to_string(),
    }
}

/// Every event on one order, oldest first.
async fn load_timeline(pool: &PgPool, order_id: Uuid) -> Result<Vec<OrderEventView>, ApiError> {
    let events = OrderEventStore::new(pool.clone())
        .load_stream(order_id)
        .await
        .map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to load order events: {err:?}"),
        })?;

    Ok(events.iter().map(view).collect())
}

fn parse_order_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "id must be a UUID".to_string(),
    })
}

#[utoipa::path(
    get, path = "/orders/{id}/events", tag = "orders",
    params(("id" = Uuid, Path, description = "Order ID")),
    responses(
        (status = 200, description = "The order's audit trail, oldest event first", body = [OrderEventView]),
        (status = 400, description = "Invalid UUID"),
        (status = 403, description = "No view grant for principal/portfolio"),
        (status = 404, description = "Order not found"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn get_order_events(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(order_id): Path<String>,
) -> Result<Json<Vec<OrderEventView>>, ApiError> {
    let order_id = parse_order_id(&order_id)?;
    // Also answers "does this order exist" — 404 for an unknown id, 403 for one
    // this principal may not see.
    require_order_grant(state.pool(), auth.principal_id, order_id, OrderPermission::View).await?;

    load_timeline(state.pool(), order_id).await.map(Json)
}

#[utoipa::path(
    get, path = "/admin/orders/{id}/events", tag = "admin",
    params(("id" = Uuid, Path, description = "Order ID")),
    responses(
        (status = 200, description = "The order's audit trail, oldest event first", body = [OrderEventView]),
        (status = 400, description = "Invalid UUID"),
        (status = 404, description = "Order not found"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn get_order_events_admin(
    State(state): State<AppState>,
    Path(order_id): Path<String>,
) -> Result<Json<Vec<OrderEventView>>, ApiError> {
    let order_id = parse_order_id(&order_id)?;

    // Oversight sees every order, so there is no grant to check — but an unknown id
    // must still 404 rather than return an empty trail.
    let exists: Option<bool> =
        query_scalar("SELECT true FROM order_state WHERE order_id = $1")
            .bind(order_id)
            .fetch_optional(state.pool())
            .await
            .map_err(|err| ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("failed to look up order: {err:?}"),
            })?;
    if exists.is_none() {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "order not found".to_string(),
        });
    }

    load_timeline(state.pool(), order_id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orders::events::OrderEventType;
    use crate::domain::orders::state::{OrderSide, OrderStatus, OrderType, TimeInForce};
    use chrono::Utc;
    use uuid::Uuid;

    fn event(payload: OrderEventPayload, event_type: OrderEventType, status: OrderStatus) -> OrderDomainEvent {
        OrderDomainEvent {
            event_id: "e-1".to_string(),
            event_type,
            order_id: "o-1".to_string(),
            timestamp: Utc::now(),
            actor: "test".to_string(),
            payload,
            version: 1,
            status_after: status,
        }
    }

    fn record_of(event: &OrderDomainEvent) -> OrderEventRecord {
        // Exactly what `handlers::domain_event_to_new_event` persists: the whole
        // domain event serialized into `payload_json`.
        OrderEventRecord {
            global_position: 7,
            order_id: Uuid::nil(),
            version: event.version,
            event_id: event.event_id.clone(),
            event_type: event.event_type.as_str().to_string(),
            actor: event.actor.clone(),
            payload: serde_json::to_value(event).expect("serialize domain event"),
            correlation_id: None,
            causation_id: None,
            schema_version: 0,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn a_stored_event_becomes_a_timeline_entry() {
        let e = event(
            OrderEventPayload::OrderRouted {
                venue: "XNAS".to_string(),
                external_order_id: "9912".to_string(),
            },
            OrderEventType::OrderRouted,
            OrderStatus::Routed,
        );
        let record = record_of(&e);

        let v = view(&record);

        assert_eq!(v.version, e.version);
        assert_eq!(v.event_id, "e-1");
        assert_eq!(v.event_type, "order_routed");
        assert_eq!(v.actor, "test");
        assert_eq!(v.occurred_at, e.timestamp);
        assert_eq!(v.recorded_at, record.created_at);
        assert_eq!(v.status_after.as_deref(), Some("routed"));
        assert_eq!(v.summary, "routed to XNAS as 9912");
        assert_eq!(v.payload["venue"], "XNAS");
    }

    #[test]
    fn a_payload_that_no_longer_parses_still_yields_a_row() {
        let record = OrderEventRecord {
            global_position: 8,
            order_id: Uuid::nil(),
            version: 2,
            event_id: "e-2".to_string(),
            event_type: "order_teleported".to_string(),
            actor: "some-broker".to_string(),
            payload: serde_json::json!({ "shape": "from a schema we no longer know" }),
            correlation_id: None,
            causation_id: None,
            schema_version: 99,
            created_at: Utc::now(),
        };

        let v = view(&record);

        assert_eq!(v.event_type, "order_teleported");
        assert_eq!(v.actor, "some-broker");
        assert_eq!(v.status_after, None);
        assert_eq!(v.summary, "order_teleported (payload not readable at schema version 99)");
        assert_eq!(v.payload["shape"], "from a schema we no longer know");
    }

    #[test]
    fn an_expiry_without_a_reason_says_only_that_it_expired() {
        let e = event(
            OrderEventPayload::OrderExpired { reason: None },
            OrderEventType::OrderExpired,
            OrderStatus::Expired,
        );

        assert_eq!(summarize(&e), "expired");
    }

    #[test]
    fn an_expiry_with_a_reason_carries_it() {
        let e = event(
            OrderEventPayload::OrderExpired {
                reason: Some("day order, session closed".to_string()),
            },
            OrderEventType::OrderExpired,
            OrderStatus::Expired,
        );

        assert_eq!(summarize(&e), "expired: day order, session closed");
    }

    #[test]
    fn a_suspension_names_the_status_it_would_resume_to() {
        let e = event(
            OrderEventPayload::OrderSuspended {
                reason: Some("trading halted".to_string()),
                resume_to_status: OrderStatus::Routed,
            },
            OrderEventType::OrderSuspended,
            OrderStatus::Suspended,
        );

        assert_eq!(summarize(&e), "suspended: trading halted (resumes to routed)");
    }

    #[test]
    fn a_suspension_without_a_reason_still_names_the_resume_status() {
        let e = event(
            OrderEventPayload::OrderSuspended {
                reason: None,
                resume_to_status: OrderStatus::PartiallyFilled,
            },
            OrderEventType::OrderSuspended,
            OrderStatus::Suspended,
        );

        assert_eq!(summarize(&e), "suspended (resumes to partially_filled)");
    }

    #[test]
    fn a_release_names_the_status_it_returned_to() {
        let e = event(
            OrderEventPayload::OrderReleased {
                resumed_to_status: OrderStatus::Routed,
            },
            OrderEventType::OrderReleased,
            OrderStatus::Routed,
        );

        assert_eq!(summarize(&e), "released to routed");
    }

    #[test]
    fn an_amendment_reports_every_field_that_moved() {
        let e = event(
            OrderEventPayload::OrderAmended {
                previous_limit_price: Some(190.02),
                new_limit_price: Some(189.50),
                previous_quantity: 100.0,
                new_quantity: 50.0,
            },
            OrderEventType::OrderAmended,
            OrderStatus::Routed,
        );

        assert_eq!(summarize(&e), "amended quantity 100 \u{2192} 50, limit 190.02 \u{2192} 189.5");
    }

    #[test]
    fn an_amendment_stays_silent_about_fields_that_did_not_move() {
        let e = event(
            OrderEventPayload::OrderAmended {
                previous_limit_price: Some(190.02),
                new_limit_price: Some(190.02),
                previous_quantity: 100.0,
                new_quantity: 50.0,
            },
            OrderEventType::OrderAmended,
            OrderStatus::Routed,
        );

        assert_eq!(summarize(&e), "amended quantity 100 \u{2192} 50");
    }

    #[test]
    fn an_amendment_that_changed_nothing_still_reads_as_an_amendment() {
        let e = event(
            OrderEventPayload::OrderAmended {
                previous_limit_price: None,
                new_limit_price: None,
                previous_quantity: 100.0,
                new_quantity: 100.0,
            },
            OrderEventType::OrderAmended,
            OrderStatus::Routed,
        );

        assert_eq!(summarize(&e), "amended (no change)");
    }

    #[test]
    fn a_cancel_without_a_reason_says_only_that_it_was_canceled() {
        let e = event(
            OrderEventPayload::OrderCanceled { reason: None },
            OrderEventType::OrderCanceled,
            OrderStatus::Canceled,
        );

        assert_eq!(summarize(&e), "canceled");
    }

    #[test]
    fn a_cancel_with_a_reason_carries_it() {
        let e = event(
            OrderEventPayload::OrderCanceled {
                reason: Some("pulled by trader".to_string()),
            },
            OrderEventType::OrderCanceled,
            OrderStatus::Canceled,
        );

        assert_eq!(summarize(&e), "canceled: pulled by trader");
    }

    #[test]
    fn a_refused_cancel_is_not_mistaken_for_a_cancel() {
        let e = event(
            OrderEventPayload::CancelRejected {
                reason: "already filled".to_string(),
            },
            OrderEventType::CancelRejected,
            OrderStatus::Filled,
        );

        assert_eq!(summarize(&e), "cancel rejected: already filled");
    }

    #[test]
    fn a_limit_submission_names_side_quantity_instrument_and_price() {
        let e = event(
            OrderEventPayload::OrderSubmitted {
                client_order_id: "c-1".to_string(),
                portfolio_id: "p-1".to_string(),
                account_id: "a-1".to_string(),
                instrument_id: "AAPL@XNAS".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::Day,
                limit_price: Some(190.02),
                quantity: 100.0,
            },
            OrderEventType::OrderSubmitted,
            OrderStatus::Submitted,
        );

        assert_eq!(summarize(&e), "submitted buy 100 AAPL@XNAS limit 190.02 (day)");
    }

    #[test]
    fn a_market_submission_carries_no_price() {
        let e = event(
            OrderEventPayload::OrderSubmitted {
                client_order_id: "c-1".to_string(),
                portfolio_id: "p-1".to_string(),
                account_id: "a-1".to_string(),
                instrument_id: "BTCUSDT@BINANCE".to_string(),
                side: OrderSide::Sell,
                order_type: OrderType::Market,
                time_in_force: TimeInForce::Ioc,
                limit_price: None,
                quantity: 0.5,
            },
            OrderEventType::OrderSubmitted,
            OrderStatus::Submitted,
        );

        assert_eq!(summarize(&e), "submitted sell 0.5 BTCUSDT@BINANCE market (ioc)");
    }

    #[test]
    fn a_rejection_leads_with_its_reason() {
        let e = event(
            OrderEventPayload::OrderRejected {
                reason: "notional limit breached".to_string(),
            },
            OrderEventType::OrderRejected,
            OrderStatus::Rejected,
        );

        assert_eq!(summarize(&e), "rejected: notional limit breached");
    }

    #[test]
    fn routing_records_the_venue_and_the_brokers_own_id() {
        let e = event(
            OrderEventPayload::OrderRouted {
                venue: "XNAS".to_string(),
                external_order_id: "9912".to_string(),
            },
            OrderEventType::OrderRouted,
            OrderStatus::Routed,
        );

        assert_eq!(summarize(&e), "routed to XNAS as 9912");
    }

    #[test]
    fn a_fill_reads_as_quantity_price_and_venue() {
        let e = event(
            OrderEventPayload::OrderFilled {
                execution_id: "x-1".to_string(),
                fill_qty: 60.0,
                fill_price: 190.05,
                cum_qty: 100.0,
                leaves_qty: 0.0,
                avg_px: 190.03,
                venue: "XNAS".to_string(),
            },
            OrderEventType::OrderFilled,
            OrderStatus::Filled,
        );

        assert_eq!(summarize(&e), "filled 60 @ 190.05 on XNAS (100 cumulative, avg 190.03)");
    }

    #[test]
    fn a_partial_fill_reports_what_is_still_working() {
        let e = event(
            OrderEventPayload::OrderPartiallyFilled {
                execution_id: "x-1".to_string(),
                fill_qty: 40.0,
                fill_price: 190.02,
                cum_qty: 40.0,
                leaves_qty: 60.0,
                avg_px: 190.02,
                venue: "XNAS".to_string(),
            },
            OrderEventType::OrderPartiallyFilled,
            OrderStatus::PartiallyFilled,
        );

        assert_eq!(
            summarize(&e),
            "partially filled 40 @ 190.02 on XNAS (40 cumulative, 60 working, avg 190.02)"
        );
    }
}
