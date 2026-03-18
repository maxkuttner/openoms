# Order Domain (Phase 1)

This module contains pure write-model domain logic for OMS orders.

## Design Intent

- Domain code is deterministic and side-effect free.
- `OrderAggregate::decide` validates a command and emits domain event(s).
- `OrderAggregate::apply` folds events into `OrderAggregateState`.
- Infrastructure concerns (DB, Kafka, HTTP, broker APIs) stay out of this module.

## Files

- `commands.rs`: canonical write-model commands.
- `events.rs`: immutable domain events + event envelope.
- `state.rs`: aggregate state shape and lifecycle status.
- `invariants.rs`: command preconditions and transition guards.
- `errors.rs`: standard rejection codes/messages.
- `aggregate.rs`: rehydrate/decide/apply logic + transition tests.

## Core Invariants

- Submit quantity must be greater than zero.
- Limit orders require a positive limit price.
- Non-submit commands require an existing stream.
- Terminal orders (`filled`, `rejected`, `canceled`, `expired`) reject mutating commands.
- Fill reports require positive qty/price and cannot exceed remaining quantity.

## Lifecycle Notes

- `SubmitOrder` creates `OrderSubmitted` with version `1`.
- Fill reports emit `OrderPartiallyFilled` or `OrderFilled` based on `leaves_qty`.
- `SuspendOrder`/`ReleaseOrder` are modeled explicitly.
- `OrderReleased` is an event, not a long-lived status.
- On suspend, state stores `resume_to_status` (the pre-suspend active status).
- On release, aggregate restores `resume_to_status` and then clears it.
- Rehydrate must produce the same final state as live event application.

## Command Rejection Policy (Integration Points)

When the write model rejects a command, we do NOT persist a domain event.

- `OrderAggregate::decide(...)` returns `Err(CommandRejection)`.
- Integration code must treat that as a normal business rejection: return an error to the caller and stop.
- Only when `decide(...)` returns `Ok(Vec<OrderDomainEvent>)` should integration append events to the event store and publish them.

This keeps the event log focused on accepted domain facts.

Important distinction:

- Command rejection: the aggregate refuses to emit events (no append).
- External fact: the outside world tells us something happened; we model it as an event and persist it.
  Example: `ReceiveExecutionReport::Reject { .. }` produces `OrderRejected` and transitions the order to a terminal state.

## Lifecycle Transition Matrix

The aggregate enforces a small, explicit "allowed command per current status" matrix.

| Current status | Allowed commands |
| --- | --- |
| (no stream) | `SubmitOrder` |
| `submitted` | `ReplaceOrder`, `CancelOrder`, `SuspendOrder`, `ReceiveExecutionReport`, `ExpireOrder` |
| `routed` | `ReplaceOrder`, `CancelOrder`, `SuspendOrder`, `ReceiveExecutionReport`, `ExpireOrder` |
| `partially_filled` | `ReplaceOrder`, `CancelOrder`, `SuspendOrder`, `ReceiveExecutionReport`, `ExpireOrder` |
| `suspended` | `CancelOrder`, `ReleaseOrder`, `ReceiveExecutionReport`, `ExpireOrder` |
| `filled` | (none) |
| `rejected` | (none) |
| `canceled` | (none) |
| `expired` | (none) |

Notes:

- Terminal statuses (`filled`, `rejected`, `canceled`, `expired`) reject all commands with `InvalidStateTransition`.
- `ReleaseOrder` is only allowed from `suspended`.
- `SuspendOrder` rejects when already `suspended`.
- `ReplaceOrder` rejects while `suspended`.
