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
        let current_status = self.state.as_ref().map(|s| s.status);
        lifecycle::ensure_command_allowed(current_status, kind)?;

        match command {
            OrderCommand::SubmitOrder(cmd) => {
                invariants::validate_submit(cmd.quantity, cmd.order_type, cmd.limit_price)?;

                Ok(vec![OrderDomainEvent {
                    event_id: metadata.event_id,
                    event_type: OrderEventType::OrderSubmitted,
                    order_id: cmd.order_id,
                    timestamp: metadata.timestamp,
                    actor: metadata.actor,
                    payload: OrderEventPayload::OrderSubmitted {
                        client_order_id: cmd.client_order_id,
                        book_id: cmd.book_id,
                        account_id: cmd.account_id,
                        instrument_id: cmd.instrument_id,
                        side: cmd.side,
                        order_type: cmd.order_type,
                        time_in_force: cmd.time_in_force,
                        limit_price: cmd.limit_price,
                        quantity: cmd.quantity,
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
                            payload: OrderEventPayload::OrderRejected { reason },
                            version: next_version,
                            status_after: OrderStatus::Rejected,
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

                        if next_leaves > 0.0 {
                            Ok(vec![OrderDomainEvent {
                                event_id: metadata.event_id,
                                event_type: OrderEventType::OrderPartiallyFilled,
                                order_id: cmd.order_id,
                                timestamp: metadata.timestamp,
                                actor: metadata.actor,
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
                                status_after: OrderStatus::PartiallyFilled,
                            }])
                        } else {
                            Ok(vec![OrderDomainEvent {
                                event_id: metadata.event_id,
                                event_type: OrderEventType::OrderFilled,
                                order_id: cmd.order_id,
                                timestamp: metadata.timestamp,
                                actor: metadata.actor,
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
                                status_after: OrderStatus::Filled,
                            }])
                        }
                    }
                }
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
                book_id,
                account_id,
                instrument_id,
                side,
                order_type,
                time_in_force,
                limit_price,
                quantity,
            } => {
                self.state = Some(OrderAggregateState {
                    order_id: event.order_id.clone(),
                    client_order_id: client_order_id.clone(),
                    book_id: book_id.clone(),
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
                    book_id: "b-2".to_string(),
                    account_id: "a-2".to_string(),
                    instrument_id: "i-2".to_string(),
                    side: OrderSide::Buy,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::Day,
                    limit_price: Some(100.0),
                    quantity: 10.0,
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
        };

        let err = aggregate
            .apply(&event)
            .expect_err("apply should fail before submit");
        assert_eq!(err.code, RejectionCode::OrderNotFound);
    }
}
