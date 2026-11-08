//! Recovery barrier: establish the stream first, replay the durable journal,
//! then reconcile complete history/snapshots and only then hand back evidence
//! for the runner to decide readiness.

use colo_link::v2::Bootstrap;
use gateway::adapter::BrokerError;
use gateway::runtime::{
    AssetBalance, ExecutionCursor, RuntimeOrder, RuntimeVenue, Trade, VenueEvent,
};
use tokio::sync::mpsc;

use crate::book::Book;
use crate::journal::{Journal, JournalRecord};
use crate::RuntimeError;

const MAX_RECONCILE_ATTEMPTS: usize = 3;
const MAX_HISTORY_PAGES: usize = 100;

pub struct RecoveredBook {
    pub book: Book,
    pub events: mpsc::Receiver<VenueEvent>,
    pub execution_cursor: ExecutionCursor,
    pub balances: Vec<AssetBalance>,
    pub orders: Vec<RuntimeOrder>,
    /// Why the recovery is not complete; empty means it is safe to consider
    /// acknowledging recovery.
    pub evidence: Vec<String>,
    pub complete: bool,
}

pub async fn recover(
    venue: &dyn RuntimeVenue,
    journal: &mut Journal,
    bootstrap: &Bootstrap,
) -> Result<RecoveredBook, RuntimeError> {
    bootstrap
        .validate()
        .map_err(|e| RuntimeError::IncompleteRecovery(e.to_string()))?;

    let replay = journal.replay()?;
    let mut cursor = bootstrap.baseline.execution_cursor.clone();
    for record in &replay {
        if let JournalRecord::Venue { event: VenueEvent::Trade(trade) } = &record.record {
            if let Some(position) = cursor.positions.get_mut(&trade.key.symbol) {
                if let Ok(id) = trade.key.trade_id.parse::<i64>() {
                    position.last_trade_id = Some(
                        position.last_trade_id.map_or(id, |current| current.max(id)),
                    );
                    position.start_time = None;
                }
            }
        }
    }

    let mut book = Book::new(journal.identity().clone(), bootstrap.limits.clone())?;
    book.install_instruments(bootstrap.instruments.clone())?;
    let opening: Vec<AssetBalance> = bootstrap
        .asset_baseline
        .iter()
        .map(|(asset, quantity)| AssetBalance {
            asset: asset.clone(),
            free: *quantity,
            locked: 0.0,
        })
        .collect();
    book.install_balances(&opening);
    for (symbol, quantity) in &bootstrap.baseline.positions {
        book.install_position(symbol.clone(), *quantity)?;
    }
    for record in &replay {
        match &record.record {
            JournalRecord::Intent { submission, intent, reservation } => {
                book.restore_order(submission, intent, reservation)?;
            }
            JournalRecord::Venue { event } => {
                book.apply(event)?;
            }
            _ => {}
        }
    }

    let (tx, mut rx) = mpsc::channel(4096);
    venue
        .subscribe(tx)
        .await
        .map_err(|e| RuntimeError::IncompleteRecovery(format!("stream not established: {e}")))?;

    reconcile_history(venue, &mut book, &mut cursor).await?;
    let has_trades = replay.iter().any(|r| {
        matches!(&r.record, JournalRecord::Venue { event: VenueEvent::Trade(_) })
    });

    let mut stable = false;
    let mut last_orders: Option<Vec<RuntimeOrder>> = None;
    let mut last_balances: Option<Vec<AssetBalance>> = None;
    let mut orders = Vec::new();
    let mut balances = Vec::new();
    for _ in 0..MAX_RECONCILE_ATTEMPTS {
        orders = venue
            .snapshot_orders()
            .await
            .map_err(|e| RuntimeError::IncompleteRecovery(format!("order snapshot failed: {e}")))?;
        balances = venue
            .balances()
            .await
            .map_err(|e| RuntimeError::IncompleteRecovery(format!("balance snapshot failed: {e}")))?;
        if !has_trades {
            check_inventory(bootstrap, &balances)?;
        }
        book.install_balances(&balances);
        for order in &orders {
            book.apply(&VenueEvent::Order(order.clone()))?;
        }
        drain(&mut rx, &mut book)?;
        if last_orders.as_ref() == Some(&orders) && last_balances.as_ref() == Some(&balances) {
            stable = true;
            break;
        }
        last_orders = Some(orders.clone());
        last_balances = Some(balances.clone());
    }
    if !stable {
        return Err(RuntimeError::IncompleteRecovery(
            "venue snapshots did not stabilise".into(),
        ));
    }
    drain(&mut rx, &mut book)?;

    let mut evidence = Vec::new();
    for client_id in book.pending_client_ids() {
        if orders.iter().any(|o| o.client_order_id == client_id) {
            continue;
        }
        let history_has_trade = replay.iter().any(|r| {
            matches!(&r.record, JournalRecord::Venue { event: VenueEvent::Trade(Trade { client_order_id, .. }) } if *client_order_id == client_id)
        });
        let Some(symbol) = book.order_symbol(&client_id) else {
            evidence.push(format!("order {client_id} has no instrument"));
            continue;
        };
        match venue.lookup(&client_id, symbol).await {
            Ok(order) => {
                book.apply(&VenueEvent::Order(order))?;
            }
            Err(BrokerError::NoSuchOrder(_)) if history_has_trade => {}
            Err(BrokerError::NoSuchOrder(_)) => {
                evidence.push(format!("pending order {client_id} has no venue evidence"));
            }
            Err(e) => {
                evidence.push(format!("lookup {client_id} failed: {e}"));
            }
        }
    }
    if let Some(killed) = bootstrap.killed_epoch {
        book.set_trading_state_halted();
        evidence.push(format!("kill latch retained from epoch {killed}"));
    }
    let complete = evidence.is_empty();
    Ok(RecoveredBook { book, events: rx, execution_cursor: cursor, balances, orders, evidence, complete })
}

async fn reconcile_history(
    venue: &dyn RuntimeVenue,
    book: &mut Book,
    cursor: &mut ExecutionCursor,
) -> Result<(), RuntimeError> {
    let mut complete = false;
    for _ in 0..MAX_HISTORY_PAGES {
        let page = venue
            .executions(cursor)
            .await
            .map_err(|e| RuntimeError::IncompleteRecovery(format!("execution history failed: {e}")))?;
        if !page.missing.is_empty() {
            return Err(RuntimeError::IncompleteRecovery(format!(
                "history missing for {}",
                page.missing.join(", ")
            )));
        }
        for trade in page.trades {
            book.apply(&VenueEvent::Trade(trade))?;
        }
        *cursor = page.next;
        if page.complete {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(RuntimeError::IncompleteRecovery(
            "execution history did not complete within the page bound".into(),
        ));
    }
    Ok(())
}

fn drain(rx: &mut mpsc::Receiver<VenueEvent>, book: &mut Book) -> Result<(), RuntimeError> {
    loop {
        match rx.try_recv() {
            Ok(event) => {
                book.apply(&event)?;
            }
            Err(mpsc::error::TryRecvError::Empty) | Err(mpsc::error::TryRecvError::Disconnected) => {
                return Ok(());
            }
        }
    }
}

fn check_inventory(bootstrap: &Bootstrap, balances: &[AssetBalance]) -> Result<(), RuntimeError> {
    for (asset, expected) in &bootstrap.asset_baseline {
        let actual = balances
            .iter()
            .find(|b| b.asset == *asset)
            .map_or(0.0, AssetBalance::total);
        if (actual - expected).abs() > 1e-9 {
            return Err(RuntimeError::IncompleteRecovery(format!(
                "{asset} inventory {actual} differs from approved baseline {expected}"
            )));
        }
    }
    Ok(())
}
