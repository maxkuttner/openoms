//! Apply one venue event: gather what the OMS knows, `decide`, then adopt,
//! book or record a break. Fills are booked exactly once per
//! `(connection, account, exec_id)`: the `venue_fill` row and the fill event
//! commit in one transaction, and every event for one venue order runs under
//! the same advisory lock, so a stream and a pull of the same fill serialize
//! and the second sees the first's row.

use chrono::Utc;
use serde_json::json;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};
use tracing::{debug, info, warn};
use uuid::Uuid;

use gateway::adapter::InstrumentProvider;
use gateway::venue_event::{VenueEvent, VenueOrderStatus, VenueOrderUpdate};

use super::adopt;
use super::decide::{decide, BreakKind, ConnPolicy, Decision, KnownOrder, Lookup, Policy, Rule};
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{
    ExecutionReport, ExpireOrder, OrderCommand, ReceiveExecutionReport, ReplaceOrder,
};
use crate::domain::orders::events::{OrderDomainEvent, OrderEventPayload};
use crate::domain::orders::state::{OrderAggregateState, OrderStatus};
use crate::event_bus::{publish_appended, EventBus, OrderLaneEvent};
use crate::event_store::{AppendResult, EventStoreError, NewOrderEvent, OrderEventStore};
use crate::execution::load_state;
use crate::fees::{book_fee, FeeBooked, FeeCharge};
use crate::handlers::domain_event_to_new_event;
use crate::order_entry::{insert_order_state, Origin};
use crate::positions;

const EPS: f64 = 1e-9;
const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug)]
pub enum IngestError {
    Db(sqlx::Error),
    /// No account of the connection matches the event's account reference.
    UnknownAccount { connection: String, account_ref: String },
    Other(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Db(e) => write!(f, "database: {e}"),
            IngestError::UnknownAccount { connection, account_ref } => {
                write!(f, "no account {account_ref} on connection {connection}")
            }
            IngestError::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for IngestError {}

impl From<sqlx::Error> for IngestError {
    fn from(e: sqlx::Error) -> Self {
        IngestError::Db(e)
    }
}

/// An attempt lost a race on the order's version; run it again.
enum Attempt {
    Done(Decision, Option<Booked>),
    Retry,
}

/// What a committed booking publishes, and the fee its fill carried.
struct Booked {
    order_id: Uuid,
    new_events: Vec<NewOrderEvent>,
    appended: AppendResult,
    position: Option<(Uuid, String)>,
    fee: Option<FeeCharge>,
}

/// What `ingest` did with one event.
#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    pub decision: Decision,
    /// The venue reports more executed on the order than the OMS has booked:
    /// its missing fills need pulling (they book with their own exec ids).
    pub fill_gap: bool,
}

impl Ingested {
    fn plain(decision: Decision) -> Self {
        Ingested { decision, fill_gap: false }
    }
}

/// Apply one venue event of `connection`. A `Duplicate` wrote nothing.
pub async fn ingest(
    pool: &PgPool,
    bus: &EventBus,
    connection: &str,
    event: VenueEvent,
    instruments: Option<&dyn InstrumentProvider>,
) -> Result<Ingested, IngestError> {
    let account_ref = match &event {
        VenueEvent::Fill(f) => f.account_ref.clone(),
        VenueEvent::Order(o) => o.account_ref.clone(),
        VenueEvent::Fee { exec_id, account_ref, amount, currency } => {
            let account = resolve_account(pool, connection, account_ref).await?;
            ingest_fee(pool, bus, connection, account, exec_id, *amount, currency).await?;
            return Ok(Ingested::plain(Decision::Ignore));
        }
        VenueEvent::Connected | VenueEvent::Disconnected => return Ok(Ingested::plain(Decision::Ignore)),
    };
    let account = resolve_account(pool, connection, &account_ref).await?;
    let policy = load_policy(pool, connection).await?;
    let broker = broker_code(pool, connection).await?;
    for attempt in 1..=MAX_ATTEMPTS {
        match attempt_once(pool, connection, &broker, account, &event, &policy, instruments).await? {
            Attempt::Done(decision, booked) => {
                if let Some(b) = booked {
                    publish(bus, &b);
                    if let Some(fee) = b.fee {
                        book_fill_fee(pool, bus, b.order_id, fee, connection).await;
                    }
                }
                let fill_gap = match &event {
                    VenueEvent::Order(o) => fill_gap(pool, connection, account, o).await?,
                    _ => false,
                };
                return Ok(Ingested { decision, fill_gap });
            }
            Attempt::Retry => {
                warn!(connection, attempt, "ingest: order version moved, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(20 * attempt as u64)).await;
            }
        }
    }
    Err(IngestError::Other(format!("ingest: gave up after {MAX_ATTEMPTS} version conflicts")))
}

/// Whether the venue's cum for the order is ahead of what the OMS booked on it.
async fn fill_gap(pool: &PgPool, connection: &str, account: Uuid, o: &VenueOrderUpdate) -> Result<bool, IngestError> {
    let by_client = o.client_order_id.as_deref().and_then(|c| Uuid::parse_str(c).ok());
    let booked: Option<f64> = sqlx::query_scalar(
        "SELECT cum_qty::float8 FROM order_state WHERE broker_connection_code = $1 \
         AND (order_id = $4 OR (account_id = $2 AND external_order_id = $3 AND venue_trade_id IS NULL)) \
         ORDER BY order_id = $4 DESC NULLS LAST LIMIT 1",
    )
    .bind(connection)
    .bind(account)
    .bind(&o.venue_order_id)
    .bind(by_client)
    .fetch_optional(pool)
    .await?;
    Ok(booked.is_some_and(|cum| o.cum_qty > cum + EPS))
}

fn actor(connection: &str) -> String {
    format!("ingest:{connection}")
}

/// The account behind `(connection, account_ref)`; a connection with a single
/// account maps every reference to it.
async fn resolve_account(pool: &PgPool, connection: &str, account_ref: &str) -> Result<Uuid, IngestError> {
    let exact: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM account WHERE broker_connection_code = $1 AND external_account_ref = $2")
            .bind(connection)
            .bind(account_ref)
            .fetch_optional(pool)
            .await?;
    if let Some(id) = exact {
        return Ok(id);
    }
    let all: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM account WHERE broker_connection_code = $1 LIMIT 2")
        .bind(connection)
        .fetch_all(pool)
        .await?;
    match all.as_slice() {
        [only] => Ok(*only),
        _ => Err(IngestError::UnknownAccount { connection: connection.into(), account_ref: account_ref.into() }),
    }
}

async fn broker_code(pool: &PgPool, connection: &str) -> Result<String, IngestError> {
    Ok(sqlx::query_scalar("SELECT broker_code FROM broker_connection WHERE code = $1")
        .bind(connection)
        .fetch_one(pool)
        .await?)
}

/// The connection's external-order policy, default book and attribution rules.
pub async fn load_policy(pool: &PgPool, connection: &str) -> Result<ConnPolicy, sqlx::Error> {
    let row = sqlx::query("SELECT external_policy, default_portfolio_id FROM broker_connection WHERE code = $1")
        .bind(connection)
        .fetch_one(pool)
        .await?;
    let policy = match row.get::<String, _>("external_policy").as_str() {
        "break" => Policy::Break,
        _ => Policy::Adopt,
    };
    let rules = sqlx::query(
        "SELECT account_id, client_id_prefix, portfolio_id, priority FROM attribution_rule \
         WHERE broker_connection_code = $1 ORDER BY priority",
    )
    .bind(connection)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| Rule {
        account_id: r.get("account_id"),
        client_id_prefix: r.get("client_id_prefix"),
        portfolio_id: r.get("portfolio_id"),
        priority: r.get("priority"),
    })
    .collect();
    Ok(ConnPolicy { policy, default_portfolio: row.get("default_portfolio_id"), rules })
}

fn ids(event: &VenueEvent) -> (&str, Option<&str>, Option<&str>) {
    match event {
        VenueEvent::Fill(f) => (&f.venue_order_id, f.client_order_id.as_deref(), Some(&f.exec_id)),
        VenueEvent::Order(o) => (&o.venue_order_id, o.client_order_id.as_deref(), None),
        _ => unreachable!("only orders and fills reach the lookup"),
    }
}

const KNOWN_SQL: &str = "SELECT order_id, original_qty::float8 AS qty, cum_qty::float8 AS cum, origin, status \
                         FROM order_state ";

fn known(row: sqlx::postgres::PgRow) -> KnownOrder {
    let status: String = row.get("status");
    KnownOrder {
        order_id: row.get("order_id"),
        qty: row.get("qty"),
        cum_qty: row.get("cum"),
        external: row.get::<String, _>("origin") == "EXTERNAL",
        terminal: matches!(status.as_str(), "filled" | "canceled" | "rejected" | "expired"),
    }
}

/// What the OMS already knows about the event's order, fill and break.
pub async fn load_lookup(
    conn: &mut PgConnection,
    connection: &str,
    account_id: Uuid,
    event: &VenueEvent,
) -> Result<Lookup, sqlx::Error> {
    let (venue_order_id, client_id, exec_id) = ids(event);
    let by_client_uuid = match client_id.and_then(|c| Uuid::parse_str(c).ok()) {
        Some(id) => sqlx::query(&format!("{KNOWN_SQL} WHERE order_id = $1 AND broker_connection_code = $2"))
            .bind(id)
            .bind(connection)
            .fetch_optional(&mut *conn)
            .await?
            .map(known),
        None => None,
    };
    let by_venue_id = sqlx::query(&format!(
        "{KNOWN_SQL} WHERE broker_connection_code = $1 AND account_id = $2 AND external_order_id = $3 \
         AND venue_trade_id IS NULL"
    ))
    .bind(connection)
    .bind(account_id)
    .bind(venue_order_id)
    .fetch_optional(&mut *conn)
    .await?
    .map(known);
    let fill_seen = match exec_id {
        Some(exec) => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM venue_fill WHERE broker_connection_code = $1 AND account_id = $2 AND exec_id = $3)",
        )
        .bind(connection)
        .bind(account_id)
        .bind(exec)
        .fetch_one(&mut *conn)
        .await?,
        None => false,
    };
    let open_break = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM recon_break WHERE broker_connection_code = $1 AND account_id = $2 \
         AND venue_order_id = $3 AND status = 'open')",
    )
    .bind(connection)
    .bind(account_id)
    .bind(venue_order_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Lookup { by_client_uuid, by_venue_id, fill_seen, open_break })
}

async fn attempt_once(
    pool: &PgPool,
    connection: &str,
    broker: &str,
    account: Uuid,
    event: &VenueEvent,
    policy: &ConnPolicy,
    instruments: Option<&dyn InstrumentProvider>,
) -> Result<Attempt, IngestError> {
    let (venue_order_id, _, _) = ids(event);
    let mut tx = pool.begin().await?;
    // every event of one venue order, from any path, one at a time
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("venue-order/{connection}/{account}/{venue_order_id}"))
        .execute(&mut *tx)
        .await?;
    let lookup = load_lookup(&mut tx, connection, account, event).await?;
    let decision = decide(event, account, &lookup, policy);
    debug!(connection, venue_order_id, ?decision, "ingest: decided");
    let ctx = Ctx { pool, connection, broker, account };
    let is_break = matches!(decision, Decision::AttachToBreak | Decision::Break(_));
    let outcome = match &decision {
        Decision::Duplicate | Decision::Ignore => return Ok(Attempt::Done(decision, None)),
        Decision::AttachToBreak => {
            record_break(&mut tx, &ctx, venue_order_id, None, event).await?;
            Attempt::Done(decision.clone(), None)
        }
        Decision::Break(kind) => {
            record_break(&mut tx, &ctx, venue_order_id, Some(*kind), event).await?;
            Attempt::Done(decision.clone(), None)
        }
        Decision::Adopt { portfolio_id } => {
            let instrument = match adopt::ensure_instrument(pool, broker, instruments, symbol(event)).await? {
                Some(id) => id,
                None => {
                    let kind = BreakKind::UnknownInstrument;
                    record_break(&mut tx, &ctx, venue_order_id, Some(kind), event).await?;
                    tx.commit().await?;
                    return Ok(Attempt::Done(Decision::Break(kind), None));
                }
            };
            let Some(principal) = adopt::owner(&mut tx, *portfolio_id).await? else {
                warn!(connection, %portfolio_id, "ingest: no principal to own an adopted order");
                let kind = BreakKind::NoBook;
                record_break(&mut tx, &ctx, venue_order_id, Some(kind), event).await?;
                tx.commit().await?;
                return Ok(Attempt::Done(Decision::Break(kind), None));
            };
            let order_id = Uuid::new_v4();
            let mut commands = adopt::commands(order_id, *portfolio_id, account, instrument, broker, event);
            commands.extend(event_commands(order_id, broker, event));
            book(&mut tx, &ctx, order_id, None, commands, Some(principal), event).await?.attempt(decision.clone())
        }
        Decision::Apply { order_id } | Decision::Grow { order_id, .. } => {
            let order_id = *order_id;
            let Some(state) = load_state(&mut tx, order_id).await.map_err(|e| IngestError::Other(e.to_string()))? else {
                return Err(IngestError::Other(format!("order {order_id} vanished")));
            };
            let terminal = state.status.is_terminal();
            let reopenable = state.status == OrderStatus::Filled && lookup_external(&lookup, order_id);
            let mut commands = Vec::new();
            match (event, &decision) {
                // a closed order the venue says executed more: the venue is truth,
                // but not beyond the order's quantity
                (VenueEvent::Fill(_), Decision::Grow { .. }) if terminal && !reopenable => {
                    let kind = BreakKind::Overfill;
                    record_break(&mut tx, &ctx, venue_order_id, Some(kind), event).await?;
                    tx.commit().await?;
                    return Ok(Attempt::Done(Decision::Break(kind), None));
                }
                // nothing more to learn from a closed order's status
                (VenueEvent::Order(_), Decision::Apply { .. }) if terminal => {
                    return Ok(Attempt::Done(decision, None));
                }
                (_, Decision::Grow { new_qty, .. }) => commands.push(OrderCommand::ReplaceOrder(ReplaceOrder {
                    order_id: order_id.to_string(),
                    new_limit_price: None,
                    new_quantity: Some(*new_qty),
                })),
                _ => {}
            }
            commands.extend(event_commands(order_id, broker, event));
            if commands.is_empty() {
                return Ok(Attempt::Done(decision, None));
            }
            book(&mut tx, &ctx, order_id, Some(state), commands, None, event).await?.attempt(decision.clone())
        }
    };
    // a retry or a duplicate drops the transaction: nothing is written
    if !matches!(outcome, Attempt::Done(_, Some(_))) && !is_break {
        return Ok(outcome);
    }
    tx.commit().await?;
    Ok(outcome)
}

fn lookup_external(lookup: &Lookup, order_id: Uuid) -> bool {
    [&lookup.by_client_uuid, &lookup.by_venue_id]
        .into_iter()
        .flatten()
        .any(|k| k.order_id == order_id && k.external)
}

enum BookOutcome {
    Booked(Booked),
    /// The order's version moved under us.
    Retry,
    /// The fill is already in `venue_fill`.
    Duplicate,
}

impl BookOutcome {
    fn attempt(self, decision: Decision) -> Attempt {
        match self {
            BookOutcome::Booked(b) => Attempt::Done(decision, Some(b)),
            BookOutcome::Retry => Attempt::Retry,
            BookOutcome::Duplicate => Attempt::Done(Decision::Duplicate, None),
        }
    }
}

struct Ctx<'a> {
    pool: &'a PgPool,
    connection: &'a str,
    broker: &'a str,
    account: Uuid,
}

fn symbol(event: &VenueEvent) -> &str {
    match event {
        VenueEvent::Fill(f) => &f.symbol,
        VenueEvent::Order(o) => &o.symbol,
        _ => "",
    }
}

/// The order's state-machine commands for one event. An order update only
/// moves status: fills book only from fill events, with their exec ids, so a
/// venue cum ahead of the books is a gap for a pull to close, never a
/// synthetic fill.
fn event_commands(order_id: Uuid, venue: &str, event: &VenueEvent) -> Vec<OrderCommand> {
    let report = |report| {
        OrderCommand::ReceiveExecutionReport(ReceiveExecutionReport { order_id: order_id.to_string(), report })
    };
    match event {
        VenueEvent::Fill(f) => vec![report(ExecutionReport::Fill {
            execution_id: f.exec_id.clone(),
            fill_qty: f.qty,
            fill_price: f.price,
            venue: venue.into(),
        })],
        VenueEvent::Order(o) => {
            let mut out = Vec::new();
            match &o.status {
                VenueOrderStatus::Canceled => {
                    out.push(report(ExecutionReport::Canceled { reason: None, venue: Some(venue.into()) }))
                }
                VenueOrderStatus::Rejected { reason } => {
                    out.push(report(ExecutionReport::Reject { reason: reason.clone(), venue: Some(venue.into()) }))
                }
                VenueOrderStatus::Expired => {
                    out.push(OrderCommand::ExpireOrder(ExpireOrder { order_id: order_id.to_string(), reason: None }))
                }
                VenueOrderStatus::Filled | VenueOrderStatus::Working => {}
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Decide and apply `commands` to the order (a new one when `base` is `None`),
/// and persist the result: the order row, the venue fill, positions and events.
#[allow(clippy::too_many_arguments)]
async fn book(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &Ctx<'_>,
    order_id: Uuid,
    base: Option<OrderAggregateState>,
    commands: Vec<OrderCommand>,
    principal: Option<Uuid>,
    event: &VenueEvent,
) -> Result<BookOutcome, IngestError> {
    let actor = actor(ctx.connection);
    let expected_version = base.as_ref().map_or(0, |s| s.version);
    let was = base.as_ref().map(|s| s.status);
    // The venue is truth: what it reports on a closed order (a late fill, a
    // filled external order growing) still books, so reopen it for the state
    // machine, which refuses every command on a terminal order.
    let base = base.map(|mut s| {
        if s.status.is_terminal() {
            s.status = if s.cum_qty > EPS { OrderStatus::PartiallyFilled } else { OrderStatus::Routed };
        }
        s
    });
    let mut aggregate = base.map_or_else(OrderAggregate::empty, OrderAggregate::from_state);
    let mut events: Vec<OrderDomainEvent> = Vec::new();
    for command in commands {
        let metadata = EventMetadata { event_id: Uuid::new_v4().to_string(), timestamp: Utc::now(), actor: actor.clone() };
        let mut decided = aggregate
            .decide(command, metadata)
            .map_err(|r| IngestError::Other(format!("order {order_id}: {}", r.message)))?;
        for e in &mut decided {
            // a late fill on a canceled/expired/rejected order books, but the
            // order stays closed, in its events as in order_state
            if let Some(prev) = was.filter(|s| s.is_terminal() && *s != OrderStatus::Filled) {
                e.status_after = prev;
            }
            aggregate.apply(e).map_err(|r| IngestError::Other(r.message))?;
        }
        events.extend(decided);
    }
    let state = aggregate.state.ok_or_else(|| IngestError::Other("no order state".into()))?;

    match principal {
        Some(principal) => {
            insert_order_state(tx, order_id, &state, principal, ctx.connection, &Origin::External, &None)
                .await
                .map_err(|e| IngestError::Other(e.message))?;
            sqlx::query("UPDATE order_state SET external_order_id = $2 WHERE order_id = $1")
                .bind(order_id)
                .bind(ids(event).0)
                .execute(&mut **tx)
                .await?;
        }
        None => {
            let updated = sqlx::query(
                "UPDATE order_state SET status = $2, original_qty = $3, leaves_qty = $4, cum_qty = $5, avg_px = $6, \
                 limit_price = $7, version = $8, updated_at = now() WHERE order_id = $1 AND version = $9",
            )
            .bind(order_id)
            .bind(state.status.as_str())
            .bind(state.original_qty)
            .bind(state.leaves_qty)
            .bind(state.cum_qty)
            .bind(state.avg_px)
            .bind(state.limit_price)
            .bind(state.version)
            .bind(expected_version)
            .execute(&mut **tx)
            .await?;
            if updated.rows_affected() == 0 {
                return Ok(BookOutcome::Retry);
            }
        }
    }

    let mut fee = None;
    if let VenueEvent::Fill(f) = event {
        let inserted = sqlx::query(
            "INSERT INTO venue_fill (broker_connection_code, account_id, exec_id, order_id) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(ctx.connection)
        .bind(ctx.account)
        .bind(&f.exec_id)
        .bind(order_id)
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() == 0 {
            // the lock keeps the lookup right; this is the backstop. The caller
            // drops the transaction, so nothing of this booking is written.
            return Ok(BookOutcome::Duplicate);
        }
        fee = f.fee.as_ref().and_then(|(amount, ccy)| fee_charge(ctx.broker, &f.exec_id, *amount, ccy));
    }

    let portfolio = Uuid::parse_str(&state.portfolio_id).map_err(|e| IngestError::Other(e.to_string()))?;
    let mut any_fill = false;
    for e in &events {
        if let OrderEventPayload::OrderPartiallyFilled { fill_qty, fill_price, .. }
        | OrderEventPayload::OrderFilled { fill_qty, fill_price, .. } = &e.payload
        {
            any_fill = true;
            positions::persist_fill(tx, portfolio, &state.instrument_id, state.side, *fill_qty, *fill_price).await?;
        }
    }
    let new_events = events
        .iter()
        .map(domain_event_to_new_event)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| IngestError::Other(e.message))?;
    let store = OrderEventStore::new(ctx.pool.clone());
    let appended = match store.append_events_in_tx(tx, order_id, expected_version, &new_events).await {
        Ok(a) => a,
        Err(EventStoreError::Concurrency(_)) => return Ok(BookOutcome::Retry),
        Err(EventStoreError::Sqlx(e)) => return Err(e.into()),
    };
    info!(%order_id, status = state.status.as_str(), connection = ctx.connection, "ingest: booked");
    Ok(BookOutcome::Booked(Booked {
        order_id,
        new_events,
        appended,
        position: any_fill.then(|| (portfolio, state.instrument_id.clone())),
        fee,
    }))
}

fn publish(bus: &EventBus, b: &Booked) {
    publish_appended(bus, b.order_id, &b.new_events, &b.appended);
    if let Some((portfolio_id, instrument_id)) = &b.position {
        bus.publish_order(OrderLaneEvent::PositionChanged { portfolio_id: *portfolio_id, instrument_id: instrument_id.clone() });
    }
}

/// A fee keyed as the session streams key it, so either path books it once.
fn fee_charge(broker: &str, exec_id: &str, amount: f64, currency: &str) -> Option<FeeCharge> {
    (amount != 0.0 && !currency.is_empty()).then(|| FeeCharge {
        fee_id: format!("{broker}-{exec_id}"),
        execution_id: Some(exec_id.to_string()),
        amount,
        currency: currency.to_string(),
    })
}

async fn book_fill_fee(pool: &PgPool, bus: &EventBus, order_id: Uuid, fee: FeeCharge, connection: &str) {
    match book_fee(pool, Some(bus), order_id, fee, &actor(connection)).await {
        Ok(FeeBooked::Booked) => {}
        Ok(other) => debug!(%order_id, ?other, "ingest: fee not booked"),
        Err(e) => tracing::error!(%order_id, error = %e, "ingest: could not book fee"),
    }
}

/// A fee reported apart from its fill: booked on the fill's order, once. Before
/// the fill is booked there is no order to charge; the fill brings it later.
async fn ingest_fee(
    pool: &PgPool,
    bus: &EventBus,
    connection: &str,
    account: Uuid,
    exec_id: &str,
    amount: f64,
    currency: &str,
) -> Result<(), IngestError> {
    let order: Option<Uuid> = sqlx::query_scalar(
        "SELECT order_id FROM venue_fill WHERE broker_connection_code = $1 AND account_id = $2 AND exec_id = $3",
    )
    .bind(connection)
    .bind(account)
    .bind(exec_id)
    .fetch_optional(pool)
    .await?;
    let Some(order_id) = order else {
        warn!(connection, exec_id, "ingest: fee before its fill; the fill will bring it");
        return Ok(());
    };
    let broker = broker_code(pool, connection).await?;
    if let Some(fee) = fee_charge(&broker, exec_id, amount, currency) {
        book_fill_fee(pool, bus, order_id, fee, connection).await;
    }
    Ok(())
}

fn break_kind(kind: BreakKind) -> &'static str {
    match kind {
        BreakKind::PolicyBreak => "policy_break",
        BreakKind::NoBook => "no_book",
        BreakKind::Overfill => "overfill",
        BreakKind::UnknownInstrument => "unknown_instrument",
    }
}

fn event_json(event: &VenueEvent) -> serde_json::Value {
    match event {
        VenueEvent::Fill(f) => json!({
            "type": "fill", "exec_id": f.exec_id, "venue_order_id": f.venue_order_id,
            "client_order_id": f.client_order_id, "account_ref": f.account_ref, "symbol": f.symbol,
            "buy": f.buy, "qty": f.qty, "price": f.price,
            "fee": f.fee.as_ref().map(|(a, c)| json!({"amount": a, "currency": c})),
            "executed_at": f.executed_at,
        }),
        VenueEvent::Order(o) => json!({
            "type": "order", "venue_order_id": o.venue_order_id, "client_order_id": o.client_order_id,
            "account_ref": o.account_ref, "symbol": o.symbol, "buy": o.buy, "order_type": o.order_type,
            "qty": o.qty, "limit_px": o.limit_px, "status": format!("{:?}", o.status), "cum_qty": o.cum_qty,
            "ts": o.ts,
        }),
        _ => json!({}),
    }
}

/// Add the event to the venue order's open break, opening one of `kind` when
/// there is none. A fill already in the break is not added again.
async fn record_break(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &Ctx<'_>,
    venue_order_id: &str,
    kind: Option<BreakKind>,
    event: &VenueEvent,
) -> Result<(), IngestError> {
    let ev = event_json(event);
    let dedup = match event {
        VenueEvent::Fill(f) => json!([{ "exec_id": f.exec_id }]),
        _ => json!([{ "never": true }]),
    };
    sqlx::query(
        "INSERT INTO recon_break (id, broker_connection_code, account_id, venue_order_id, kind, events_json) \
         VALUES ($1, $2, $3, $4, $5, jsonb_build_array($6::jsonb)) \
         ON CONFLICT (broker_connection_code, account_id, venue_order_id) WHERE status = 'open' \
         DO UPDATE SET events_json = recon_break.events_json || EXCLUDED.events_json \
         WHERE NOT recon_break.events_json @> $7::jsonb",
    )
    .bind(Uuid::new_v4())
    .bind(ctx.connection)
    .bind(ctx.account)
    .bind(venue_order_id)
    .bind(break_kind(kind.unwrap_or(BreakKind::NoBook)))
    .bind(&ev)
    .bind(&dedup)
    .execute(&mut **tx)
    .await?;
    warn!(connection = ctx.connection, venue_order_id, ?kind, "ingest: recorded in a break");
    Ok(())
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
