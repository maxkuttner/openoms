use chrono::{DateTime, Utc};

use super::commands::{ExecutionReport, OrderCommand};
use super::errors::{CommandRejection, RejectionCode};
use super::events::{OrderDomainEvent, OrderEventPayload, OrderEventType};
use super::invariants;
use super::lifecycle;
use super::state::{OrderAggregateState, OrderStatus};

#[derive(Debug, Clone)]
pub struct EventMetadata {
    pub event_id: String,
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    /// Venue time of what the command reports; copied onto every event.
    pub occurred_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
pub struct OrderAggregate {
    // the state can either be Some or None
    pub state: Option<OrderAggregateState>,
}

impl OrderAggregate {
    // create an empty state
    pub fn empty() -> Self {
        Self { state: None }
    }

    // create a new state machine (aka order aggregate) from a given state
    // note: at this point state and aggregate state are kind of the same but not
    pub fn from_state(state: OrderAggregateState) -> Self {
        Self { state: Some(state) }
    }

    pub fn rehydrate(events: &[OrderDomainEvent]) -> Result<Self, CommandRejection> {
        let mut aggregate = Self::default();
        for event in events {
            aggregate.apply(event)?;
        }
        Ok(aggregate)
    }

    // Method to decide which events ought to be issued.
    // Note: The method returns a vector of events, since
    // it is possible that in the future commands are modelled
    // as a ordered list of of domain events.
    pub fn decide(
        &self,
        command: OrderCommand,
        metadata: EventMetadata,
    ) -> Result<Vec<OrderDomainEvent>, CommandRejection> {
        // Command rejection policy:
        // if we return Err(CommandRejection) we emit NO domain events.
        // Integration code should only append/publish when we return Ok(events).
        let kind = command.kind();
        let current_status = self.state.as_ref().map(|s| {
            // An observed order's status is the venue's, ahead of its booked
            // fills: until booked cum reaches quantity it still takes fills
            // (the fill keeps the terminal status, see below).
            if s.observed && s.status.is_terminal() && is_fill(&command) && s.cum_qty < s.original_qty {
                OrderStatus::PartiallyFilled
            } else {
                s.status
            }
        });
        lifecycle::ensure_command_allowed(current_status, kind)?;

        match command {
            OrderCommand::SubmitOrder(cmd) => {
                invariants::validate_priced_submit(cmd.quantity, cmd.order_type, cmd.limit_price, cmd.spread_price)?;

                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderSubmitted,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderSubmitted {
                        client_order_id: cmd.client_order_id,
                        portfolio_id: cmd.portfolio_id,
                        account_id: cmd.account_id,
                        instrument_id: cmd.instrument_id,
                        side: cmd.side,
                        order_type: cmd.order_type,
                        time_in_force: cmd.time_in_force,
                        limit_price: cmd.limit_price,
                        quantity: cmd.quantity,
                        parent_order_id: cmd.parent_order_id,
                    },
                    version: 1,
                    status_after: OrderStatus::Submitted,
                }])
            }
            OrderCommand::RouteOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;
                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderRouted,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderRouted {
                        venue: cmd.venue,
                        external_order_id: cmd.external_order_id,
                    },
                    version: next_version,
                    status_after: OrderStatus::Routed,
                }])
            }
            OrderCommand::ReplaceOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                let new_quantity = cmd.new_quantity.unwrap_or(state.original_qty);
                if new_quantity <= 0.0 || new_quantity < state.cum_qty {
                    return Err(CommandRejection::new(
                        RejectionCode::InvalidQuantity,
                        "new quantity must be positive and cannot be less than cumulative fill",
                    ));
                }

                if let Some(px) = cmd.new_limit_price {
                    if px <= 0.0 {
                        return Err(CommandRejection::new(
                            RejectionCode::InvalidPrice,
                            "new limit price must be positive",
                        ));
                    }
                }

                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderAmended,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderAmended {
                        previous_limit_price: state.limit_price,
                        new_limit_price: cmd.new_limit_price.or(state.limit_price),
                        previous_quantity: state.original_qty,
                        new_quantity,
                    },
                    version: next_version,
                    status_after: state.status,
                }])
            }
            OrderCommand::CancelOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderCanceled,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderCanceled { reason: cmd.reason },
                    version: next_version,
                    status_after: OrderStatus::Canceled,
                }])
            }
            OrderCommand::SuspendOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderSuspended,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderSuspended {
                        reason: cmd.reason,
                        resume_to_status: state.status,
                    },
                    version: next_version,
                    status_after: OrderStatus::Suspended,
                }])
            }
            OrderCommand::ReleaseOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                let resumed_to_status = state.resume_to_status.ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::InvalidStateTransition,
                        "release requires stored pre-suspend status",
                    )
                })?;

                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderReleased,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderReleased { resumed_to_status },
                    version: next_version,
                    status_after: resumed_to_status,
                }])
            }
            OrderCommand::ReceiveExecutionReport(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                match cmd.report {
                    ExecutionReport::Reject { reason, .. } => {
                        let next_version = state.version + 1;
                        Ok(vec![OrderDomainEvent {
                            event_id: metadata.event_id,
                            event_type: OrderEventType::OrderRejected,
                            order_id: cmd.order_id,
                            timestamp: metadata.timestamp,
                            actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                            payload: OrderEventPayload::OrderRejected { reason },
                            version: next_version,
                            status_after: OrderStatus::Rejected,
                        }])
                    }
                    // Broker-confirmed cancellation: the venue acked the cancel request.
                    ExecutionReport::Canceled { reason, .. } => {
                        let next_version = state.version + 1;
                        Ok(vec![OrderDomainEvent {
                            event_id: metadata.event_id,
                            event_type: OrderEventType::OrderCanceled,
                            order_id: cmd.order_id,
                            timestamp: metadata.timestamp,
                            actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                            payload: OrderEventPayload::OrderCanceled { reason },
                            version: next_version,
                            status_after: OrderStatus::Canceled,
                        }])
                    }
                    ExecutionReport::Fill {
                        execution_id,
                        fill_qty,
                        fill_price,
                        venue,
                    } => {
                        if fill_qty <= 0.0 {
                            return Err(CommandRejection::new(
                                RejectionCode::InvalidExecutionReport,
                                "fill quantity must be greater than zero",
                            ));
                        }

                        if fill_price <= 0.0 {
                            return Err(CommandRejection::new(
                                RejectionCode::InvalidExecutionReport,
                                "fill price must be greater than zero",
                            ));
                        }

                        let next_cum = state.cum_qty + fill_qty;
                        if next_cum > state.original_qty {
                            return Err(CommandRejection::new(
                                RejectionCode::InvalidExecutionReport,
                                "fill quantity exceeds remaining quantity",
                            ));
                        }

                        let next_leaves = state.original_qty - next_cum;
                        let prev_notional = state.cum_qty * state.avg_px.unwrap_or(0.0);
                        let next_avg = (prev_notional + (fill_qty * fill_price)) / next_cum;
                        let next_version = state.version + 1;
                        // a fill on an observed order the venue already
                        // reported closed books but leaves the status alone
                        let keep = (state.observed && state.status.is_terminal()).then_some(state.status);

                        if next_leaves > 0.0 {
                            Ok(vec![OrderDomainEvent {
                                event_id: metadata.event_id,
                                event_type: OrderEventType::OrderPartiallyFilled,
                                order_id: cmd.order_id,
                                timestamp: metadata.timestamp,
                                actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                                payload: OrderEventPayload::OrderPartiallyFilled {
                                    execution_id,
                                    fill_qty,
                                    fill_price,
                                    cum_qty: next_cum,
                                    leaves_qty: next_leaves,
                                    avg_px: next_avg,
                                    venue,
                                },
                                version: next_version,
                                status_after: keep.unwrap_or(OrderStatus::PartiallyFilled),
                            }])
                        } else {
                            Ok(vec![OrderDomainEvent {
                                event_id: metadata.event_id,
                                event_type: OrderEventType::OrderFilled,
                                order_id: cmd.order_id,
                                timestamp: metadata.timestamp,
                                actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                                payload: OrderEventPayload::OrderFilled {
                                    execution_id,
                                    fill_qty,
                                    fill_price,
                                    cum_qty: next_cum,
                                    leaves_qty: 0.0,
                                    avg_px: next_avg,
                                    venue,
                                },
                                version: next_version,
                                status_after: keep.unwrap_or(OrderStatus::Filled),
                            }])
                        }
                    }
                }
            }
            OrderCommand::RecordLegsFilled(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;
                if !(cmd.cum_qty.is_finite() && cmd.cum_qty > 0.0 && cmd.avg_px.is_finite()) {
                    return Err(CommandRejection::new(
                        RejectionCode::InvalidExecutionReport,
                        "legs progress needs a positive quantity and a finite price",
                    ));
                }
                if cmd.cum_qty > state.original_qty + 1e-9 || cmd.cum_qty < state.cum_qty - 1e-9 {
                    return Err(CommandRejection::new(
                        RejectionCode::InvalidExecutionReport,
                        "legs progress must lie between the booked quantity and the order quantity",
                    ));
                }
                let cum_qty = cmd.cum_qty.min(state.original_qty);
                let leaves_qty = (state.original_qty - cum_qty).max(0.0);
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderLegsFilled,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderLegsFilled { cum_qty, leaves_qty, avg_px: cmd.avg_px },
                    version: state.version + 1,
                    status_after: if leaves_qty <= 1e-9 { OrderStatus::Filled } else { OrderStatus::PartiallyFilled },
                }])
            }
            OrderCommand::ObserveOrder(cmd) => {
                invariants::validate_submit(cmd.quantity, cmd.order_type, cmd.limit_price)?;
                if matches!(cmd.observed_status, OrderStatus::Submitted | OrderStatus::Suspended) {
                    return Err(CommandRejection::new(
                        RejectionCode::InvalidStateTransition,
                        format!("{:?} is not a venue status", cmd.observed_status),
                    ));
                }
                if !(0.0..=cmd.quantity).contains(&cmd.venue_cum_qty) {
                    return Err(CommandRejection::new(
                        RejectionCode::InvalidQuantity,
                        "venue cum qty must be between 0 and quantity",
                    ));
                }
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderObserved,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderObserved {
                        client_order_id: cmd.client_order_id,
                        portfolio_id: cmd.portfolio_id,
                        account_id: cmd.account_id,
                        instrument_id: cmd.instrument_id,
                        side: cmd.side,
                        order_type: cmd.order_type,
                        time_in_force: cmd.time_in_force,
                        limit_price: cmd.limit_price,
                        quantity: cmd.quantity,
                        venue: cmd.venue,
                        external_order_id: cmd.external_order_id,
                        observed_status: cmd.observed_status,
                        venue_cum_qty: cmd.venue_cum_qty,
                        venue_created_at: cmd.venue_created_at,
                    },
                    version: 1,
                    status_after: cmd.observed_status,
                }])
            }
            OrderCommand::ExpireOrder(cmd) => {
                let state = invariants::require_existing_order(&self.state)?;

                let next_version = state.version + 1;
                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderExpired,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    occurred_at: metadata.occurred_at,
                    payload: OrderEventPayload::OrderExpired { reason: cmd.reason },
                    version: next_version,
                    status_after: OrderStatus::Expired,
                }])
            }
        }
    }

    // Method to apply the OrderDomainEvent
    // TODO: This function should ideally already assume that it is given a
    // valid and correct domain event. S.t. there is no need to issue a CommandRejection error
    //
    pub fn apply(&mut self, event: &OrderDomainEvent) -> Result<(), CommandRejection> {
        match &event.payload {
            OrderEventPayload::OrderSubmitted {
                client_order_id,
                portfolio_id,
                account_id,
                instrument_id,
                side,
                order_type,
                time_in_force,
                limit_price,
                quantity,
                ..
            } => {
                self.state = Some(OrderAggregateState {
                    order_id: event.order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    portfolio_id: portfolio_id.clone(),
                    account_id: account_id.clone(),
                    instrument_id: instrument_id.clone(),
                    side: *side,
                    order_type: *order_type,
                    time_in_force: *time_in_force,
                    limit_price: *limit_price,
                    original_qty: *quantity,
                    leaves_qty: *quantity,
                    cum_qty: 0.0,
                    avg_px: None,
                    status: event.status_after,
                    resume_to_status: None,
                    version: event.version,
                    observed: false,
                });
            }
            OrderEventPayload::OrderObserved {
                client_order_id,
                portfolio_id,
                account_id,
                instrument_id,
                side,
                order_type,
                time_in_force,
                limit_price,
                quantity,
                ..
            } => {
                self.state = Some(OrderAggregateState {
                    order_id: event.order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    portfolio_id: portfolio_id.clone(),
                    account_id: account_id.clone(),
                    instrument_id: instrument_id.clone(),
                    side: *side,
                    order_type: *order_type,
                    time_in_force: *time_in_force,
                    limit_price: *limit_price,
                    original_qty: *quantity,
                    leaves_qty: *quantity,
                    cum_qty: 0.0,
                    avg_px: None,
                    status: event.status_after,
                    resume_to_status: None,
                    version: event.version,
                    observed: true,
                });
            }
            OrderEventPayload::OrderAmended {
                new_limit_price,
                new_quantity,
                ..
            } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot amend state before submit",
                    )
                })?;

                state.limit_price = *new_limit_price;
                state.original_qty = *new_quantity;
                state.leaves_qty = *new_quantity - state.cum_qty;
                state.status = event.status_after;
                state.version = event.version;
            }
            OrderEventPayload::OrderPartiallyFilled {
                cum_qty,
                leaves_qty,
                avg_px,
                ..
            }
            | OrderEventPayload::OrderLegsFilled {
                cum_qty,
                leaves_qty,
                avg_px,
            }
            | OrderEventPayload::OrderFilled {
                cum_qty,
                leaves_qty,
                avg_px,
                ..
            } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot fill state before submit",
                    )
                })?;

                state.cum_qty = *cum_qty;
                state.leaves_qty = *leaves_qty;
                state.avg_px = Some(*avg_px);
                state.status = event.status_after;
                state.version = event.version;
            }
            OrderEventPayload::OrderSuspended {
                resume_to_status, ..
            } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot apply lifecycle event before submit",
                    )
                })?;

                state.status = event.status_after;
                state.resume_to_status = Some(*resume_to_status);
                state.version = event.version;
            }
            OrderEventPayload::OrderReleased { resumed_to_status } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot apply lifecycle event before submit",
                    )
                })?;

                state.status = *resumed_to_status;
                state.resume_to_status = None;
                state.version = event.version;
            }
            OrderEventPayload::OrderFeeCharged { .. } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot apply fee event before submit",
                    )
                })?;

                state.version = event.version;
            }
            OrderEventPayload::OrderRejected { .. }
            | OrderEventPayload::OrderCanceled { .. }
            | OrderEventPayload::OrderExpired { .. }
            | OrderEventPayload::OrderRouted { .. }
            | OrderEventPayload::CancelRejected { .. } => {
                let state = self.state.as_mut().ok_or_else(|| {
                    CommandRejection::new(
                        RejectionCode::OrderNotFound,
                        "cannot apply lifecycle event before submit",
                    )
                })?;

                if matches!(event.payload, OrderEventPayload::OrderCanceled { .. }) {
                    state.leaves_qty = 0.0;
                }

                state.status = event.status_after;
                if state.status != OrderStatus::Suspended {
                    state.resume_to_status = None;
                }
                state.version = event.version;
            }
        }

        Ok(())
    }
}

fn is_fill(command: &OrderCommand) -> bool {
    matches!(command, OrderCommand::ReceiveExecutionReport(r) if matches!(r.report, ExecutionReport::Fill { .. }))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::domain::orders::commands::{
        CancelOrder, ReceiveExecutionReport, ReleaseOrder, ReplaceOrder, SubmitOrder, SuspendOrder,
    };
    use crate::domain::orders::fixtures::{self, OrderTestData};
    use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};

    fn t() -> OrderTestData {
        OrderTestData::new("o-1")
    }

    fn submitted_aggregate() -> OrderAggregate {
        fixtures::submitted_aggregate(&t())
    }

    fn metadata(id: &str) -> EventMetadata {
        fixtures::metadata(id)
    }

    #[test]
    fn submit_with_negative_quantity_is_rejected() {
        let aggregate = OrderAggregate::default();

        let mut cmd = t().submit_limit(10.0, 100.0);
        cmd.quantity = -1.0;

        let err = aggregate
            .decide(OrderCommand::SubmitOrder(cmd), metadata("e-1"))
            .expect_err("submit should reject negative quantity");

        assert_eq!(err.code, RejectionCode::InvalidQuantity);
    }

    #[test]
    fn submit_with_zero_quantity_is_rejected() {
        let aggregate = OrderAggregate::default();

        let mut cmd = t().submit_limit(10.0, 100.0);
        cmd.quantity = 0.0;

        let err = aggregate
            .decide(OrderCommand::SubmitOrder(cmd), metadata("e-1"))
            .expect_err("submit should not succeed");
        assert_eq!(err.code, RejectionCode::InvalidQuantity);
    }

    #[test]
    fn submit_limit_without_price_is_rejected() {
        let aggregate = OrderAggregate::default();

        let cmd = t().submit(
            OrderSide::Buy,
            OrderType::Limit,
            TimeInForce::Day,
            None,
            10.0,
        );

        let err = aggregate
            .decide(OrderCommand::SubmitOrder(cmd), metadata("e-1"))
            .expect_err("submit should reject limit without price");
        assert_eq!(err.code, RejectionCode::InvalidPrice);
    }

    #[test]
    fn double_submit_is_rejected() {
        let aggregate = submitted_aggregate();

        let cmd = t().submit(
            OrderSide::Sell,
            OrderType::Market,
            TimeInForce::Day,
            None,
            10.0,
        );

        let err = aggregate
            .decide(OrderCommand::SubmitOrder(cmd), metadata("e-2"))
            .expect_err("double submit should fail");
        assert_eq!(err.code, RejectionCode::OrderAlreadyExists);
    }

    #[test]
    fn fee_event_bumps_version_only() {
        use crate::fees::FeeEffect;
        let mut aggregate = submitted_aggregate();
        let fill = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(t().exec_fill("x-1", 10.0, 101.0, "XNYS")),
                metadata("e-2"),
            )
            .expect("fill should succeed");
        aggregate.apply(&fill[0]).expect("apply should succeed");
        let before = aggregate.state.clone().expect("state");
        assert_eq!(before.status, OrderStatus::Filled);

        let fee = OrderDomainEvent {
            event_id: "e-3".into(),
            event_type: OrderEventType::OrderFeeCharged,
            order_id: before.order_id.clone(),
            timestamp: Utc::now(),
            actor: "test".into(),
            payload: OrderEventPayload::OrderFeeCharged {
                fee_id: "f-1".into(),
                execution_id: Some("x-1".into()),
                amount: 0.5,
                currency: "USD".into(),
                effect: FeeEffect::Quote,
            },
            version: before.version + 1,
            status_after: before.status,
            occurred_at: None,
        };
        aggregate.apply(&fee).expect("apply should succeed");
        let after = aggregate.state.expect("state");
        assert_eq!(after.version, before.version + 1);
        assert_eq!(after.status, before.status);
        assert_eq!(after.cum_qty, before.cum_qty);
        assert_eq!(after.avg_px, before.avg_px);
    }

    #[test]
    fn submit_then_partial_fill_then_fill_updates_state() {
        let mut aggregate = submitted_aggregate();

        let partial_events = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(t().exec_fill("x-1", 4.0, 101.0, "XNYS")),
                metadata("e-2"),
            )
            .expect("partial fill should succeed");
        aggregate
            .apply(partial_events.first().expect("event must exist"))
            .expect("apply should succeed");

        let fill_events = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(t().exec_fill("x-2", 6.0, 99.0, "XNYS")),
                metadata("e-3"),
            )
            .expect("final fill should succeed");
        aggregate
            .apply(fill_events.first().expect("event must exist"))
            .expect("apply should succeed");

        let state = aggregate.state.expect("state should exist");
        assert_eq!(state.status, OrderStatus::Filled);
        assert_eq!(state.cum_qty, 10.0);
        assert_eq!(state.leaves_qty, 0.0);
        assert!(state.avg_px.is_some());
    }

    #[test]
    fn cancel_after_fill_is_rejected() {
        let mut aggregate = submitted_aggregate();
        let fill_event = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-1".to_string(),
                        fill_qty: 10.0,
                        fill_price: 100.0,
                        venue: "XNAS".to_string(),
                    },
                }),
                metadata("e-2"),
            )
            .expect("fill should succeed");
        aggregate
            .apply(fill_event.first().expect("event must exist"))
            .expect("apply should succeed");

        let err = aggregate
            .decide(
                OrderCommand::CancelOrder(CancelOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("too late".to_string()),
                }),
                metadata("e-3"),
            )
            .expect_err("cancel must be rejected");
        assert_eq!(err.code, RejectionCode::InvalidStateTransition);
    }

    #[test]
    fn broker_canceled_report_finalizes_cancellation() {
        // A broker-confirmed cancellation arrives as an execution report and
        // drives the terminal OrderCanceled (the stream is the source of truth).
        let mut aggregate = submitted_aggregate();
        let events = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Canceled {
                        reason: Some("broker canceled".to_string()),
                        venue: Some("ALPACA".to_string()),
                    },
                }),
                metadata("e-2"),
            )
            .expect("canceled report should produce an event");
        aggregate
            .apply(events.first().expect("event must exist"))
            .expect("apply should succeed");

        let state = aggregate.state.expect("state should exist");
        assert_eq!(state.status, OrderStatus::Canceled);
    }

    #[test]
    fn replace_below_cumulative_quantity_is_rejected() {
        let mut aggregate = submitted_aggregate();
        let partial = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-1".to_string(),
                        fill_qty: 7.0,
                        fill_price: 100.0,
                        venue: "XNAS".to_string(),
                    },
                }),
                metadata("e-2"),
            )
            .expect("fill should succeed");
        aggregate
            .apply(partial.first().expect("event must exist"))
            .expect("apply should succeed");

        let err = aggregate
            .decide(
                OrderCommand::ReplaceOrder(ReplaceOrder {
                    order_id: "o-1".to_string(),
                    new_limit_price: Some(99.5),
                    new_quantity: Some(5.0),
                }),
                metadata("e-3"),
            )
            .expect_err("replace must be rejected");
        assert_eq!(err.code, RejectionCode::InvalidQuantity);
    }

    #[test]
    fn suspend_then_release_is_allowed() {
        let mut aggregate = submitted_aggregate();
        let suspend = aggregate
            .decide(
                OrderCommand::SuspendOrder(SuspendOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("manual hold".to_string()),
                }),
                metadata("e-2"),
            )
            .expect("suspend should succeed");
        aggregate
            .apply(suspend.first().expect("event must exist"))
            .expect("apply should succeed");

        let release = aggregate
            .decide(
                OrderCommand::ReleaseOrder(crate::domain::orders::commands::ReleaseOrder {
                    order_id: "o-1".to_string(),
                }),
                metadata("e-3"),
            )
            .expect("release should succeed");
        aggregate
            .apply(release.first().expect("event must exist"))
            .expect("apply should succeed");

        let state = aggregate.state.expect("state should exist");
        assert_eq!(state.status, OrderStatus::Submitted);
        assert_eq!(state.resume_to_status, None);
    }

    #[test]
    fn release_restores_routed_when_suspended_from_routed() {
        let mut aggregate = submitted_aggregate();
        let routed_event = OrderDomainEvent {
            event_id: "e-route".to_string(),
            event_type: OrderEventType::OrderRouted,
            order_id: "o-1".to_string(),
            timestamp: Utc::now(),
            actor: "test".to_string(),
            payload: OrderEventPayload::OrderRouted {
                venue: "XNAS".to_string(),
                external_order_id: "test-ext-id".to_string(),
            },
            version: 2,
            status_after: OrderStatus::Routed,
            occurred_at: None,
        };
        aggregate
            .apply(&routed_event)
            .expect("route apply should succeed");

        let suspend = aggregate
            .decide(
                OrderCommand::SuspendOrder(SuspendOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("venue maintenance".to_string()),
                }),
                metadata("e-3"),
            )
            .expect("suspend should succeed");
        aggregate
            .apply(suspend.first().expect("event must exist"))
            .expect("apply should succeed");

        let release = aggregate
            .decide(
                OrderCommand::ReleaseOrder(ReleaseOrder {
                    order_id: "o-1".to_string(),
                }),
                metadata("e-4"),
            )
            .expect("release should succeed");
        aggregate
            .apply(release.first().expect("event must exist"))
            .expect("apply should succeed");

        let state = aggregate.state.expect("state should exist");
        assert_eq!(state.status, OrderStatus::Routed);
        assert_eq!(state.resume_to_status, None);
    }

    #[test]
    fn release_without_suspend_is_rejected() {
        let aggregate = submitted_aggregate();
        let err = aggregate
            .decide(
                OrderCommand::ReleaseOrder(ReleaseOrder {
                    order_id: "o-1".to_string(),
                }),
                metadata("e-2"),
            )
            .expect_err("release should fail when not suspended");
        assert_eq!(err.code, RejectionCode::CommandNotAllowed);
    }

    #[test]
    fn suspend_when_already_suspended_is_rejected() {
        let mut aggregate = submitted_aggregate();
        let suspend = aggregate
            .decide(
                OrderCommand::SuspendOrder(SuspendOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("manual hold".to_string()),
                }),
                metadata("e-2"),
            )
            .expect("first suspend should succeed");
        aggregate
            .apply(suspend.first().expect("event must exist"))
            .expect("apply should succeed");

        let err = aggregate
            .decide(
                OrderCommand::SuspendOrder(SuspendOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("second hold".to_string()),
                }),
                metadata("e-3"),
            )
            .expect_err("second suspend should fail");
        assert_eq!(err.code, RejectionCode::CommandNotAllowed);
    }

    #[test]
    fn replace_while_suspended_is_rejected() {
        let mut aggregate = submitted_aggregate();
        let suspend = aggregate
            .decide(
                OrderCommand::SuspendOrder(SuspendOrder {
                    order_id: "o-1".to_string(),
                    reason: Some("manual hold".to_string()),
                }),
                metadata("e-2"),
            )
            .expect("suspend should succeed");
        aggregate
            .apply(suspend.first().expect("event must exist"))
            .expect("apply should succeed");

        let err = aggregate
            .decide(
                OrderCommand::ReplaceOrder(ReplaceOrder {
                    order_id: "o-1".to_string(),
                    new_limit_price: Some(100.5),
                    new_quantity: Some(12.0),
                }),
                metadata("e-3"),
            )
            .expect_err("replace while suspended should fail");
        assert_eq!(err.code, RejectionCode::CommandNotAllowed);
    }

    #[test]
    fn fill_with_zero_qty_is_rejected() {
        let aggregate = submitted_aggregate();
        let err = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-1".to_string(),
                        fill_qty: 0.0,
                        fill_price: 100.0,
                        venue: "XNAS".to_string(),
                    },
                }),
                metadata("e-2"),
            )
            .expect_err("zero fill qty should fail");
        assert_eq!(err.code, RejectionCode::InvalidExecutionReport);
    }

    #[test]
    fn fill_with_zero_price_is_rejected() {
        let aggregate = submitted_aggregate();
        let err = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-1".to_string(),
                        fill_qty: 1.0,
                        fill_price: 0.0,
                        venue: "XNAS".to_string(),
                    },
                }),
                metadata("e-2"),
            )
            .expect_err("zero fill price should fail");
        assert_eq!(err.code, RejectionCode::InvalidExecutionReport);
    }

    #[test]
    fn fill_exceeding_remaining_quantity_is_rejected() {
        let aggregate = submitted_aggregate();
        let err = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-1".to_string(),
                        fill_qty: 11.0,
                        fill_price: 100.0,
                        venue: "XNAS".to_string(),
                    },
                }),
                metadata("e-2"),
            )
            .expect_err("oversized fill should fail");
        assert_eq!(err.code, RejectionCode::InvalidExecutionReport);
    }

    #[test]
    fn broker_reject_event_transitions_to_rejected_status() {
        let mut aggregate = submitted_aggregate();
        let reject_event = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-1".to_string(),
                    report: ExecutionReport::Reject {
                        reason: "venue reject".to_string(),
                        venue: Some("XNAS".to_string()),
                    },
                }),
                metadata("e-2"),
            )
            .expect("reject report should produce rejection event");

        assert_eq!(reject_event[0].event_type, OrderEventType::OrderRejected);
        aggregate
            .apply(reject_event.first().expect("event must exist"))
            .expect("apply should succeed");

        let state = aggregate.state.expect("state should exist");
        assert_eq!(state.status, OrderStatus::Rejected);
    }

    #[test]
    fn non_submit_commands_reject_on_missing_stream() {
        let aggregate = OrderAggregate::default();

        let missing = OrderTestData::new("o-missing");

        let replace_err = aggregate
            .decide(
                OrderCommand::ReplaceOrder(missing.replace(Some(100.0), Some(10.0))),
                metadata("e-replace"),
            )
            .expect_err("replace should reject missing stream");
        assert_eq!(replace_err.code, RejectionCode::OrderNotFound);

        let cancel_err = aggregate
            .decide(
                OrderCommand::CancelOrder(missing.cancel(None)),
                metadata("e-cancel"),
            )
            .expect_err("cancel should reject missing stream");
        assert_eq!(cancel_err.code, RejectionCode::OrderNotFound);

        let suspend_err = aggregate
            .decide(
                OrderCommand::SuspendOrder(missing.suspend(None)),
                metadata("e-suspend"),
            )
            .expect_err("suspend should reject missing stream");
        assert_eq!(suspend_err.code, RejectionCode::OrderNotFound);

        let release_err = aggregate
            .decide(
                OrderCommand::ReleaseOrder(missing.release()),
                metadata("e-release"),
            )
            .expect_err("release should reject missing stream");
        assert_eq!(release_err.code, RejectionCode::OrderNotFound);

        let exec_err = aggregate
            .decide(
                OrderCommand::ReceiveExecutionReport(
                    missing.exec_reject("missing order", Some("XNAS")),
                ),
                metadata("e-exec"),
            )
            .expect_err("exec report should reject missing stream");
        assert_eq!(exec_err.code, RejectionCode::OrderNotFound);

        let expire_err = aggregate
            .decide(
                OrderCommand::ExpireOrder(missing.expire(Some("expired"))),
                metadata("e-expire"),
            )
            .expect_err("expire should reject missing stream");
        assert_eq!(expire_err.code, RejectionCode::OrderNotFound);
    }

    #[test]
    fn rehydrate_with_multiple_events_restores_expected_state() {
        let initial = OrderAggregate::default();
        let submit_events = initial
            .decide(
                OrderCommand::SubmitOrder(SubmitOrder {
                    order_id: "o-2".to_string(),
                    client_order_id: "c-2".to_string(),
                    portfolio_id: "b-2".to_string(),
                    account_id: "a-2".to_string(),
                    instrument_id: "i-2".to_string(),
                    side: OrderSide::Buy,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::Day,
                    limit_price: Some(100.0),
                    quantity: 10.0,
                    parent_order_id: None,
                    spread_price: false,
                }),
                metadata("ev-1"),
            )
            .expect("submit should succeed");

        let mut stateful =
            OrderAggregate::rehydrate(&submit_events).expect("rehydrate should work");
        let amend_events = stateful
            .decide(
                OrderCommand::ReplaceOrder(ReplaceOrder {
                    order_id: "o-2".to_string(),
                    new_limit_price: Some(101.0),
                    new_quantity: Some(12.0),
                }),
                metadata("ev-2"),
            )
            .expect("replace should succeed");
        stateful
            .apply(amend_events.first().expect("event must exist"))
            .expect("apply should work");

        let fill_events = stateful
            .decide(
                OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport {
                    order_id: "o-2".to_string(),
                    report: ExecutionReport::Fill {
                        execution_id: "x-200".to_string(),
                        fill_qty: 5.0,
                        fill_price: 101.5,
                        venue: "XNYS".to_string(),
                    },
                }),
                metadata("ev-3"),
            )
            .expect("fill should succeed");

        let mut stream = Vec::new();
        stream.extend(submit_events);
        stream.extend(amend_events);
        stream.extend(fill_events);

        let aggregate = OrderAggregate::rehydrate(&stream).expect("final rehydrate should work");
        let state = aggregate.state.expect("state should exist");

        assert_eq!(state.version, 3);
        assert_eq!(state.status, OrderStatus::PartiallyFilled);
        assert_eq!(state.original_qty, 12.0);
        assert_eq!(state.leaves_qty, 7.0);
        assert_eq!(state.cum_qty, 5.0);
        assert_eq!(state.avg_px, Some(101.5));
    }

    #[test]
    fn legs_progress_moves_a_parent_without_a_fill() {
        let mut agg = submitted_aggregate();
        let legs = |cum: f64, px: f64| {
            OrderCommand::RecordLegsFilled(crate::domain::orders::commands::RecordLegsFilled {
                order_id: "o-1".into(),
                cum_qty: cum,
                avg_px: px,
            })
        };
        let qty = agg.state.as_ref().unwrap().original_qty;
        let events = agg.decide(legs(qty / 2.0, -1.25), metadata("e-2")).expect("partial legs");
        assert_eq!(events[0].event_type, OrderEventType::OrderLegsFilled);
        assert_eq!(events[0].status_after, OrderStatus::PartiallyFilled);
        agg.apply(&events[0]).unwrap();
        let s = agg.state.clone().unwrap();
        assert_eq!((s.cum_qty, s.leaves_qty, s.avg_px), (qty / 2.0, qty / 2.0, Some(-1.25)));
        // never backwards, never past the quantity
        assert!(agg.decide(legs(qty / 4.0, 1.0), metadata("e-3")).is_err());
        assert!(agg.decide(legs(qty + 1.0, 1.0), metadata("e-3")).is_err());
        let events = agg.decide(legs(qty, 2.0), metadata("e-3")).expect("all legs");
        assert_eq!(events[0].status_after, OrderStatus::Filled);
        agg.apply(&events[0]).unwrap();
        // a filled parent takes no more
        assert!(agg.decide(legs(qty, 2.0), metadata("e-4")).is_err());
    }

    #[test]
    fn lifecycle_event_before_submit_fails_apply() {
        let mut aggregate = OrderAggregate::default();
        let event = OrderDomainEvent {
            event_id: "e-bad".to_string(),
            event_type: OrderEventType::OrderCanceled,
            order_id: "o-missing".to_string(),
            timestamp: Utc::now(),
            actor: "test".to_string(),
            payload: OrderEventPayload::OrderCanceled { reason: None },
            version: 1,
            status_after: OrderStatus::Canceled,
            occurred_at: None,
        };

        let err = aggregate
            .apply(&event)
            .expect_err("apply should fail before submit");
        assert_eq!(err.code, RejectionCode::OrderNotFound);
    }

    fn observed(status: OrderStatus, venue_cum: f64) -> OrderAggregate {
        let events = OrderAggregate::default()
            .decide(OrderCommand::ObserveOrder(t().observe(10.0, status, venue_cum)), metadata("e-obs"))
            .expect("observe should succeed");
        OrderAggregate::rehydrate(&events).expect("rehydrate")
    }

    #[test]
    fn observe_creates_order_in_observed_status_with_zero_booked_cum() {
        let events = OrderAggregate::default()
            .decide(OrderCommand::ObserveOrder(t().observe(10.0, OrderStatus::Filled, 10.0)), metadata("e-obs"))
            .expect("observe should succeed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, OrderEventType::OrderObserved);
        assert_eq!(events[0].event_type.as_str(), "order_observed");
        assert_eq!(events[0].version, 1);
        assert_eq!(events[0].status_after, OrderStatus::Filled);
        match &events[0].payload {
            OrderEventPayload::OrderObserved { venue_cum_qty, external_order_id, observed_status, .. } => {
                assert_eq!(*venue_cum_qty, 10.0);
                assert_eq!(external_order_id, "x-1");
                assert_eq!(*observed_status, OrderStatus::Filled);
            }
            other => panic!("unexpected payload {other:?}"),
        }
        let state = OrderAggregate::rehydrate(&events).unwrap().state.unwrap();
        assert_eq!(state.status, OrderStatus::Filled);
        assert_eq!(state.cum_qty, 0.0);
        assert_eq!(state.leaves_qty, 10.0);
        assert_eq!(state.original_qty, 10.0);
        assert_eq!(state.avg_px, None);
        assert!(state.observed);
    }

    #[test]
    fn observe_on_existing_order_is_rejected() {
        let err = submitted_aggregate()
            .decide(OrderCommand::ObserveOrder(t().observe(10.0, OrderStatus::Routed, 0.0)), metadata("e-2"))
            .expect_err("observe on existing stream");
        assert_eq!(err.code, RejectionCode::OrderAlreadyExists);
        let err = observed(OrderStatus::Routed, 0.0)
            .decide(OrderCommand::ObserveOrder(t().observe(10.0, OrderStatus::Routed, 0.0)), metadata("e-2"))
            .expect_err("observe twice");
        assert_eq!(err.code, RejectionCode::OrderAlreadyExists);
    }

    #[test]
    fn observe_with_non_positive_quantity_is_rejected() {
        let err = OrderAggregate::default()
            .decide(OrderCommand::ObserveOrder(t().observe(0.0, OrderStatus::Routed, 0.0)), metadata("e-1"))
            .expect_err("zero qty");
        assert_eq!(err.code, RejectionCode::InvalidQuantity);
    }

    #[test]
    fn observe_with_venue_cum_out_of_range_is_rejected() {
        for cum in [-1.0, 10.5] {
            let err = OrderAggregate::default()
                .decide(OrderCommand::ObserveOrder(t().observe(10.0, OrderStatus::Filled, cum)), metadata("e-1"))
                .expect_err("venue cum out of range");
            assert_eq!(err.code, RejectionCode::InvalidQuantity);
        }
    }

    #[test]
    fn fill_after_observed_filled_is_accepted() {
        let mut agg = observed(OrderStatus::Filled, 10.0);
        for (i, qty) in [4.0, 6.0].into_iter().enumerate() {
            let events = agg
                .decide(
                    OrderCommand::ReceiveExecutionReport(t().exec_fill(&format!("x{i}"), qty, 100.0, "BINANCE")),
                    metadata(&format!("e-f{i}")),
                )
                .expect("fill on observed-filled order accepted");
            assert_eq!(events[0].status_after, OrderStatus::Filled);
            agg.apply(&events[0]).unwrap();
        }
        let s = agg.state.as_ref().unwrap();
        assert_eq!(s.cum_qty, 10.0);
        assert_eq!(s.leaves_qty, 0.0);
        assert_eq!(s.status, OrderStatus::Filled);
        // booked cum reached quantity: now really terminal
        let err = agg
            .decide(OrderCommand::ReceiveExecutionReport(t().exec_fill("x9", 1.0, 100.0, "BINANCE")), metadata("e-f9"))
            .expect_err("terminal once fully booked");
        assert_eq!(err.code, RejectionCode::InvalidStateTransition);
    }

    #[test]
    fn fill_after_observed_canceled_books_and_keeps_canceled() {
        let agg = observed(OrderStatus::Canceled, 3.0);
        let events = agg
            .decide(OrderCommand::ReceiveExecutionReport(t().exec_fill("x1", 3.0, 100.0, "BINANCE")), metadata("e-f"))
            .expect("late fill on observed-canceled order accepted");
        assert_eq!(events[0].event_type, OrderEventType::OrderPartiallyFilled);
        assert_eq!(events[0].status_after, OrderStatus::Canceled);
        // other commands still refused on the terminal order
        let err = agg
            .decide(OrderCommand::CancelOrder(t().cancel(None)), metadata("e-c"))
            .expect_err("cancel on canceled");
        assert_eq!(err.code, RejectionCode::InvalidStateTransition);
    }

    #[test]
    fn fill_on_observed_partially_filled_completes_to_filled() {
        let agg = observed(OrderStatus::PartiallyFilled, 4.0);
        let events = agg
            .decide(OrderCommand::ReceiveExecutionReport(t().exec_fill("x1", 10.0, 100.0, "BINANCE")), metadata("e-f"))
            .expect("fill");
        assert_eq!(events[0].status_after, OrderStatus::Filled);
    }

    #[test]
    fn metadata_occurred_at_is_copied_to_events() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-01T12:00:00Z").unwrap().with_timezone(&Utc);
        let mut md = metadata("e-1");
        md.occurred_at = Some(at);
        let events = OrderAggregate::default()
            .decide(OrderCommand::ObserveOrder(t().observe(10.0, OrderStatus::Routed, 0.0)), md.clone())
            .unwrap();
        assert_eq!(events[0].occurred_at, Some(at));
        let mut md2 = md.clone();
        md2.event_id = "e-2".into();
        let fills = observed(OrderStatus::Routed, 0.0)
            .decide(OrderCommand::ReceiveExecutionReport(t().exec_fill("x1", 1.0, 100.0, "BINANCE")), md2)
            .unwrap();
        assert_eq!(fills[0].occurred_at, Some(at));
        let plain = submitted_aggregate()
            .decide(OrderCommand::CancelOrder(t().cancel(None)), metadata("e-3"))
            .unwrap();
        assert_eq!(plain[0].occurred_at, None);
    }
}
