use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use sqlx::{PgPool, Row};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use std::sync::Arc;

use crate::adapters::alpaca::AlpacaAdapter;
use crate::domain::orders::aggregate::{EventMetadata, OrderAggregate};
use crate::domain::orders::commands::{ExecutionReport, OrderCommand, ReceiveExecutionReport};
use crate::domain::orders::state::OrderAggregateState;
use crate::event_store::{NewOrderEvent, OrderEventStore};
use crate::handlers::{
    parse_order_side, parse_order_status, parse_order_type, parse_time_in_force,
};
use crate::kafka::{publish_events, KafkaClient};

pub async fn run(
    environment: &'static str,
    api_key: String,
    api_secret: String,
    pool: PgPool,
    kafka: Option<KafkaClient>,
    adapter: Arc<AlpacaAdapter>,
) {
    info!(env = environment, "starting Alpaca trade-update stream");
    reconcile_routed_orders(&pool, &kafka, &adapter).await;

    let ws_url = if environment == "LIVE" {
        "wss://api.alpaca.markets/stream"
    } else {
        "wss://paper-api.alpaca.markets/stream"
    };

    let mut backoff_secs: u64 = 1;

    loop {
        info!(env = environment, attempt = backoff_secs, "Alpaca stream connecting");
        match connect_and_run(ws_url, &api_key, &api_secret, &pool, &kafka).await {
            Ok(()) => {
                warn!(env = environment, "Alpaca stream closed cleanly, reconnecting in {backoff_secs}s");
            }
            Err(e) => {
                warn!(env = environment, error = %e, "Alpaca stream error, reconnecting in {backoff_secs}s");
            }
        }
        sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn reconcile_routed_orders(pool: &PgPool, kafka: &Option<KafkaClient>, adapter: &AlpacaAdapter) {
    info!("starting reconciliation of routed orders");

    let rows = match sqlx::query("SELECT order_id FROM order_state WHERE status = 'routed'")
        .fetch_all(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => { error!(error = %e, "reconciliation: failed to query routed orders"); return; }
    };

    if rows.is_empty() {
        info!("reconciliation complete: no routed orders");
        return;
    }

    info!(count = rows.len(), "reconciliation: checking routed orders");

    for row in rows {
        let order_id: Uuid = row.get("order_id");

        // fetch the Alpaca order ID from the OrderRouted event payload
        let ext_row = sqlx::query(
            "SELECT payload_json->'payload'->>'external_order_id' AS ext_id \
             FROM order_event WHERE order_id = $1 AND event_type = 'order_routed' LIMIT 1",
        )
        .bind(order_id)
        .fetch_optional(pool)
        .await;

        let external_order_id: String = match ext_row {
            Ok(Some(r)) => match r.get::<Option<String>, _>("ext_id") {
                Some(id) if !id.is_empty() => id,
                _ => { warn!(order_id = %order_id, "reconciliation: no external_order_id found, skipping"); continue; }
            },
            Ok(None) => { warn!(order_id = %order_id, "reconciliation: OrderRouted event not found, skipping"); continue; }
            Err(e) => { error!(order_id = %order_id, error = %e, "reconciliation: DB error fetching external_order_id"); continue; }
        };

        let alpaca_order = match adapter.get_order(&external_order_id).await {
            Ok(o) => o,
            Err(e) => { warn!(order_id = %order_id, error = %e, "reconciliation: failed to fetch order from Alpaca, skipping"); continue; }
        };

        let status = alpaca_order["status"].as_str().unwrap_or("");
        let report = match status {
            "filled" | "partially_filled" => {
                let fill_qty: f64 = alpaca_order["filled_qty"]
                    .as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let fill_price: f64 = alpaca_order["filled_avg_price"]
                    .as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let execution_id = alpaca_order["id"].as_str().unwrap_or("").to_string();
                ExecutionReport::Fill { execution_id, fill_qty, fill_price, venue: "ALPACA".to_string() }
            }
            "rejected" | "canceled" | "expired" => {
                ExecutionReport::Reject {
                    reason: status.to_string(),
                    venue: Some("ALPACA".to_string()),
                }
            }
            other => {
                info!(order_id = %order_id, alpaca_status = other, "reconciliation: order still pending at Alpaca, skipping");
                continue;
            }
        };

        match process_execution_report(pool, kafka, order_id, report).await {
            Ok(()) => info!(order_id = %order_id, alpaca_status = status, "reconciliation: applied missed fill"),
            Err(e) => error!(order_id = %order_id, error = %e, "reconciliation: failed to apply execution report"),
        }
    }

    info!("reconciliation complete");
}

async fn connect_and_run(
    ws_url: &str,
    api_key: &str,
    api_secret: &str,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(ws_url).await?;
    info!("Alpaca stream connected url={ws_url}");

    let auth = serde_json::json!({
        "action": "auth",
        "key": api_key,
        "secret": api_secret,
    });
    ws.send(Message::Text(auth.to_string())).await?;

    // Subscribe
    let listen = serde_json::json!({
        "action": "listen",
        "data": { "streams": ["trade_updates"] }
    });
    ws.send(Message::Text(listen.to_string())).await?;

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => {
                match String::from_utf8(b) {
                    Ok(s) => s,
                    Err(_) => { warn!("Alpaca stream: received non-UTF8 binary frame, skipping"); continue; }
                }
            }
            Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
            Message::Close(_) => break,
            _ => continue,
        };

        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, raw = %text, "Alpaca stream: failed to parse message"); continue; }
        };

        // Alpaca wraps messages: {"stream":"...", "data":{...}}
        let stream = v["stream"].as_str().unwrap_or("");
        let data = &v["data"];

        match stream {
            "authorization" => {
                let status = data["status"].as_str().unwrap_or("");
                if status == "authorized" {
                    info!("Alpaca stream authenticated");
                } else {
                    error!(status, "Alpaca stream auth failed");
                    return Err(format!("auth failed: {status}").into());
                }
            }
            "listening" => {
                info!("Alpaca stream subscribed to trade_updates");
                // reset backoff on successful connection
            }
            "trade_updates" => {
                if let Err(e) = handle_trade_update(data, pool, kafka).await {
                    error!(error = %e, "Alpaca stream: error processing trade update");
                }
            }
            other => {
                info!(stream = other, "Alpaca stream: unhandled stream type");
            }
        }
    }

    Ok(())
}

async fn handle_trade_update(
    data: &serde_json::Value,
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event = data["event"].as_str().unwrap_or("unknown");
    let order = &data["order"];
    let client_order_id = order["client_order_id"].as_str().unwrap_or("");

    info!(event, client_order_id, "alpaca trade update received");

    let report = match event {
        "fill" | "partial_fill" => {
            let fill_qty: f64 = data["qty"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let fill_price: f64 = data["price"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let execution_id = data["execution_id"]
                .as_str()
                .unwrap_or(client_order_id)
                .to_string();

            ExecutionReport::Fill {
                execution_id,
                fill_qty,
                fill_price,
                venue: "ALPACA".to_string(),
            }
        }
        "rejected" => {
            let reason = order["status"].as_str().unwrap_or("rejected").to_string();
            ExecutionReport::Reject {
                reason,
                venue: Some("ALPACA".to_string()),
            }
        }
        "canceled" | "expired" | "pending_new" | "new" | "accepted" | "done_for_day" => {
            return Ok(());
        }
        other => {
            info!(event = other, "Alpaca stream: unhandled event type, skipping");
            return Ok(());
        }
    };

    let order_id = match Uuid::parse_str(client_order_id) {
        Ok(id) => id,
        Err(_) => {
            warn!(client_order_id, "Alpaca stream: client_order_id is not a valid UUID, skipping");
            return Ok(());
        }
    };

    match process_execution_report(pool, kafka, order_id, report).await {
        Ok(()) => {}
        Err(e) => {
            error!(order_id = %order_id, error = %e, "Alpaca stream: failed to process execution report");
        }
    }

    Ok(())
}

async fn process_execution_report(
    pool: &PgPool,
    kafka: &Option<KafkaClient>,
    order_id: Uuid,
    report: ExecutionReport,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event_store = OrderEventStore::new(pool.clone());
    let mut tx = pool.begin().await?;

    let row = sqlx::query(
        r#"
        SELECT
            order_id, client_order_id, book_id, account_id, instrument_id,
            side, order_type, time_in_force,
            limit_price::double precision AS limit_price,
            original_qty::double precision AS original_qty,
            leaves_qty::double precision AS leaves_qty,
            cum_qty::double precision AS cum_qty,
            avg_px::double precision AS avg_px,
            status, resume_to_status, version
        FROM order_state
        WHERE order_id = $1
        "#,
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;

    let row = match row {
        Some(r) => r,
        None => {
            warn!(order_id = %order_id, "Alpaca stream: order not found in DB, skipping");
            return Ok(());
        }
    };

    let current_status: &str = row.get("status");
    if matches!(current_status, "filled" | "rejected" | "canceled" | "expired") {
        warn!(order_id = %order_id, status = current_status, "Alpaca stream: order already in terminal state, skipping duplicate event");
        return Ok(());
    }

    // Reuse parse helpers from handlers — map errors to strings for the boxed error type
    let side = parse_order_side(row.get("side")).map_err(|e| e.message.clone())?;
    let order_type = parse_order_type(row.get("order_type")).map_err(|e| e.message.clone())?;
    let time_in_force = parse_time_in_force(row.get("time_in_force")).map_err(|e| e.message.clone())?;
    let status = parse_order_status(row.get("status")).map_err(|e| e.message.clone())?;
    let resume_to_status = row
        .get::<Option<String>, _>("resume_to_status")
        .map(parse_order_status)
        .transpose()
        .map_err(|e| e.message.clone())?;

    let state = OrderAggregateState {
        order_id: row.get::<Uuid, _>("order_id").to_string(),
        client_order_id: row.get("client_order_id"),
        book_id: row.get::<Uuid, _>("book_id").to_string(),
        account_id: row.get::<Uuid, _>("account_id").to_string(),
        instrument_id: row.get("instrument_id"),
        side,
        order_type,
        time_in_force,
        limit_price: row.get("limit_price"),
        original_qty: row.get("original_qty"),
        leaves_qty: row.get("leaves_qty"),
        cum_qty: row.get("cum_qty"),
        avg_px: row.get("avg_px"),
        status,
        resume_to_status,
        version: row.get("version"),
    };

    let expected_version = state.version;
    let aggregate = OrderAggregate::from_state(state);
    let metadata = EventMetadata {
        event_id: Uuid::new_v4().to_string(),
        timestamp: Utc::now(),
        actor: "alpaca".to_string(),
    };

    let cmd = ReceiveExecutionReport {
        order_id: order_id.to_string(),
        report,
    };

    let events = match aggregate.decide(OrderCommand::ReceiveExecutionReport(cmd), metadata) {
        Ok(evts) => evts,
        Err(rejection) => {
            warn!(order_id = %order_id, reason = %rejection.message, "Alpaca stream: state machine rejected execution report, skipping");
            return Ok(());
        }
    };

    let mut applied = aggregate;
    for event in &events {
        applied.apply(event).map_err(|e| format!("apply failed: {e:?}"))?;
    }

    let new_state = applied.state.ok_or("missing aggregate state after apply")?;

    sqlx::query(
        r#"
        UPDATE order_state
        SET
            status          = $2,
            leaves_qty      = $3,
            cum_qty         = $4,
            avg_px          = $5,
            version         = $6,
            updated_at      = now()
        WHERE order_id = $1 AND version = $7
        "#,
    )
    .bind(order_id)
    .bind(new_state.status.as_str())
    .bind(new_state.leaves_qty)
    .bind(new_state.cum_qty)
    .bind(new_state.avg_px)
    .bind(new_state.version)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?;

    let new_events: Vec<NewOrderEvent> = events
        .iter()
        .map(|e| {
            let payload = serde_json::to_value(e).unwrap_or_default();
            NewOrderEvent {
                event_id: e.event_id.clone(),
                event_type: e.event_type.as_str().to_string(),
                actor: e.actor.clone(),
                payload,
                correlation_id: None,
                causation_id: None,
                schema_version: 0,
            }
        })
        .collect();

    event_store
        .append_events_in_tx(&mut tx, order_id, expected_version, &new_events)
        .await
        .map_err(|e| format!("event store error: {e:?}"))?;

    tx.commit().await?;

    info!(order_id = %order_id, status = new_state.status.as_str(), "execution report applied");

    publish_events(kafka.as_ref(), &order_id.to_string(), &events).await;

    Ok(())
}
