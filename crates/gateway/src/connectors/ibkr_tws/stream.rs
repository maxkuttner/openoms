//! The IBKR_TWS execution stream and the gateway supervisor behind it.
//!
//! [`supervise`] keeps IB Gateway running and connected (Docker, then the TWS
//! API), swaps the client into the adapter's slot and runs [`run`] on it. [`run`]
//! turns the client's events into engine events; after every (re)connect it
//! checks the account, relearns order refs and replays today's executions and
//! finished orders, booking each execution once by its exec id.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};
use uuid::Uuid;

use crate::contract::{EngineHandle, ExecEvent, VenueReport};
use crate::stream_health::StreamHandle;

use super::adapter::{map_error, outcome, ClientSlot, ErrorClass, IbkrTwsAdapter};
use super::client::{IbapiClient, TwsClient, TwsEvent, TwsOrderState};
use super::gateway::{classify_logs, container_name, ensure_running, GatewayDocker, GatewayState};
use super::IbkrTwsCredentials;
use crate::contract::BrokerOrderOutcome;

const VENUE: &str = "IBKR_TWS";
const ACTOR: &str = "ibkr_tws";
/// How long to wait for the client's own reconnect before restarting from Docker.
const RECONNECT_WAIT: Duration = Duration::from_secs(120);
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A session that lasted this long was healthy: the next failure retries quickly.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// What this stream has already sent the engine, kept across reconnects.
#[derive(Default)]
pub struct Booked {
    execs: HashSet<String>,
    canceled: HashSet<i32>,
    rejected: HashSet<i32>,
}

/// Everything one session needs besides the client.
pub struct Ctx {
    pub adapter: Arc<IbkrTwsAdapter>,
    pub health: StreamHandle,
    pub engine: EngineHandle,
    pub env: &'static str,
}

fn order_uuid(adapter: &IbkrTwsAdapter, order_id: i32, order_ref: &str) -> Option<Uuid> {
    let r = if order_ref.is_empty() { adapter.order_ref(order_id)? } else { order_ref.to_string() };
    Uuid::parse_str(&r).ok()
}

async fn send(ctx: &Ctx, order: Uuid, report: VenueReport) {
    ctx.engine.exec(ExecEvent::from_report(order, VENUE, ACTOR, report)).await;
}

/// Books one client event, at most once per exec id / order.
pub async fn handle(ev: TwsEvent, ctx: &Ctx, booked: &mut Booked) {
    match ev {
        TwsEvent::Execution { exec_id, order_id, order_ref, qty, price, .. } => {
            if booked.execs.contains(&exec_id) {
                return;
            }
            let Some(order) = order_uuid(&ctx.adapter, order_id, &order_ref) else {
                warn!(exec_id, order_id, order_ref, "IBKR execution for an order the OMS does not know; skipped");
                return;
            };
            booked.execs.insert(exec_id.clone());
            send(ctx, order, VenueReport::Fill { execution_id: exec_id, fill_qty: qty, fill_price: price, venue: VENUE.into() }).await;
        }
        // The engine's fill carries no fee yet: recorded in the log only.
        TwsEvent::Commission { exec_id, amount, currency } => {
            info!(exec_id, amount, currency, "IBKR commission");
        }
        TwsEvent::Status { order_id, order_ref, status, .. } => {
            if outcome(&status) == BrokerOrderOutcome::Canceled && booked.canceled.insert(order_id) {
                match order_uuid(&ctx.adapter, order_id, &order_ref) {
                    Some(order) => send(ctx, order, VenueReport::Canceled { reason: None, venue: Some(VENUE.into()) }).await,
                    None => warn!(order_id, "IBKR cancel for an order the OMS does not know"),
                }
            }
        }
        TwsEvent::Error { order_id: Some(id), code, message } if id > 0 => match map_error(code, &message) {
            ErrorClass::Reject if booked.rejected.insert(id) => match order_uuid(&ctx.adapter, id, "") {
                Some(order) => {
                    let reason = format!("IB error {code}: {message}");
                    send(ctx, order, VenueReport::Reject { reason, venue: Some(VENUE.into()) }).await
                }
                None => warn!(order_id = id, code, message, "IBKR reject for an order the OMS does not know"),
            },
            _ => info!(order_id = id, code, message, "IBKR order notice"),
        },
        TwsEvent::Error { code, message, .. } => info!(code, message, "IBKR notice"),
        TwsEvent::Disconnected => {}
    }
}

/// Replays today's executions and the finished orders' cancels; booking is idempotent.
pub async fn catch_up(client: &dyn TwsClient, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    let completed = client.completed_orders().await.map_err(|e| e.to_string())?;
    ctx.adapter.learn(&completed);
    for ev in client.executions_today().await.map_err(|e| e.to_string())? {
        handle(ev, ctx, booked).await;
    }
    for (id, order_ref, TwsOrderState { status, filled, remaining, .. }) in completed {
        handle(TwsEvent::Status { order_id: id, order_ref, status, filled, remaining }, ctx, booked).await;
    }
    Ok(())
}

/// After a (re)connect: account check, order refs, catch-up, then live and a reconcile.
pub async fn on_connected(client: &dyn TwsClient, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    let accounts = client.managed_accounts().await.map_err(|e| e.to_string())?;
    if accounts.is_empty() {
        return Err("IBKR returned no account".into());
    }
    let open = client.open_orders().await.map_err(|e| e.to_string())?;
    ctx.adapter.learn(&open);
    catch_up(client, ctx, booked).await?;
    ctx.health.set_live();
    info!(accounts = ?accounts, "IBKR execution stream live");
    ctx.engine.reconcile(VENUE, ctx.env, ACTOR, ctx.adapter.clone()).await;
    Ok(())
}

async fn wait_reconnect(client: &dyn TwsClient, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if client.connected() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    client.connected()
}

/// Runs one connected client until it is lost for good. Rides out the
/// client's own reconnects, catching up after each.
pub async fn run(client: Arc<dyn TwsClient>, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    // Subscribe first, so nothing between the catch-up and the loop is missed.
    let mut events = client.events();
    on_connected(client.as_ref(), ctx, booked).await?;
    loop {
        match events.recv().await {
            Ok(TwsEvent::Disconnected) => {
                ctx.health.set_connecting();
                warn!("IB Gateway connection lost; waiting for it to come back");
                if !wait_reconnect(client.as_ref(), RECONNECT_WAIT).await {
                    return Err("IB Gateway connection lost".into());
                }
                on_connected(client.as_ref(), ctx, booked).await?;
            }
            Ok(TwsEvent::Error { order_id: None, code: 1100, .. }) => {
                ctx.health.set_down("IB Gateway lost its connection to IBKR");
            }
            Ok(TwsEvent::Error { order_id: None, code: code @ (1101 | 1102), .. }) => {
                info!(code, "IB Gateway reconnected to IBKR; catching up");
                catch_up(client.as_ref(), ctx, booked).await?;
                ctx.health.set_live();
            }
            Ok(ev) => {
                ctx.health.record_event();
                handle(ev, ctx, booked).await;
            }
            Err(RecvError::Lagged(n)) => {
                warn!(missed = n, "IBKR event stream lagged; catching up");
                catch_up(client.as_ref(), ctx, booked).await?;
            }
            Err(RecvError::Closed) => return Err("IBKR event stream closed".into()),
        }
    }
}

/// What the supervisor does with a gateway state it can't connect past.
#[derive(Debug, PartialEq)]
pub enum Next {
    Retry(String),
    Stop(String),
}

pub fn on_gateway_state(s: &GatewayState) -> Next {
    match s {
        GatewayState::LoginFailed(m) => Next::Stop(format!("IBKR login failed: {m}")),
        GatewayState::AwaitingTwoFactor => Next::Retry("waiting for two-factor approval on the IBKR mobile app".into()),
        GatewayState::Starting | GatewayState::Connected => Next::Retry("IB Gateway is starting".into()),
        GatewayState::Disconnected => Next::Retry("IB Gateway is disconnected".into()),
    }
}

pub fn next_backoff(d: Duration) -> Duration {
    (d * 2).min(MAX_BACKOFF)
}

/// Where the gateway lives and how to log in.
pub struct Gateway {
    pub docker: Arc<dyn GatewayDocker>,
    /// Names the container: one per connection environment.
    pub code: String,
    pub env: &'static str,
    pub creds: IbkrTwsCredentials,
    pub state_dir: PathBuf,
}

/// Keeps IB Gateway up and the stream running, backing off 1s -> 60s. A failed
/// login stops it: retrying a bad password risks locking the account.
pub async fn supervise(gw: Gateway, slot: ClientSlot, ctx: Ctx) {
    let mut backoff = FIRST_BACKOFF;
    let mut booked = Booked::default();
    loop {
        ctx.health.set_connecting();
        let started = Instant::now();
        let next = match ensure_running(gw.docker.clone(), &gw.code, gw.env, &gw.creds, &gw.state_dir).await {
            Err(state) => on_gateway_state(&state),
            Ok(port) => match IbapiClient::connect(port).await {
                Err(e) => {
                    let docker = gw.docker.clone();
                    let name = container_name(&gw.code);
                    let tail = tokio::task::spawn_blocking(move || docker.logs_tail(&name, 200)).await.unwrap_or_default();
                    match classify_logs(&tail) {
                        Some(state) => on_gateway_state(&state),
                        None => Next::Retry(format!("IB Gateway not reachable yet: {e}")),
                    }
                }
                Ok(client) => {
                    let client: Arc<dyn TwsClient> = Arc::new(client);
                    *slot.write().unwrap() = Some(client.clone());
                    let r = run(client, &ctx, &mut booked).await;
                    *slot.write().unwrap() = None;
                    Next::Retry(r.err().unwrap_or_else(|| "IBKR session ended".into()))
                }
            },
        };
        match next {
            Next::Stop(m) => {
                warn!(error = %m, "IBKR_TWS stream stopped");
                ctx.health.set_down(m);
                return;
            }
            Next::Retry(m) => {
                if started.elapsed() >= STABLE_AFTER {
                    backoff = FIRST_BACKOFF;
                }
                warn!(error = %m, "IBKR_TWS: retrying in {}s", backoff.as_secs());
                ctx.health.set_down(m);
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;

    use super::super::adapter::tests::rig;
    use super::super::client::{OrderKind, Side, Tif, TwsOrder};
    use super::*;
    use crate::contract::{ExecKind, GatewayMsg};
    use crate::stream_health::{StreamHealthRegistry, StreamKind};

    fn ctx(adapter: Arc<IbkrTwsAdapter>) -> (Ctx, tokio::sync::mpsc::Receiver<GatewayMsg>, StreamHealthRegistry) {
        let reg = StreamHealthRegistry::new();
        let health = reg.handle(VENUE, "PAPER", StreamKind::Execution);
        let (engine, rx) = EngineHandle::new();
        (Ctx { adapter, health, engine, env: "PAPER" }, rx, reg)
    }

    fn exec(exec_id: &str, order_id: i32, order_ref: &str) -> TwsEvent {
        TwsEvent::Execution { exec_id: exec_id.into(), order_id, order_ref: order_ref.into(), qty: 3.0, price: 190.25, time: Utc::now() }
    }

    fn fills(rx: &mut tokio::sync::mpsc::Receiver<GatewayMsg>) -> Vec<(Uuid, ExecKind)> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let GatewayMsg::Exec(e) = m {
                out.push((e.order_id, e.kind));
            }
        }
        out
    }

    #[tokio::test]
    async fn a_fill_for_an_order_from_before_a_restart_maps_by_order_ref() {
        let (_, adapter) = rig();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let id = Uuid::new_v4();
        handle(exec("e1", 42, &id.to_string()), &ctx, &mut Booked::default()).await;
        assert_eq!(fills(&mut rx), vec![(id, ExecKind::Fill { execution_id: "e1".into(), qty: 3.0, price: 190.25 })]);
    }

    #[tokio::test]
    async fn a_fill_without_a_ref_maps_by_the_ibkr_order_id() {
        let (fake, adapter) = rig();
        let id = Uuid::new_v4();
        let state = TwsOrderState {
            status: "Submitted".into(),
            filled: 0.0,
            remaining: 3.0,
            avg_fill_price: 0.0,
            symbol: "AAPL".into(),
            order: TwsOrder {
                order_id: 42,
                con_id: 265598,
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Buy,
                qty: 3.0,
                order_type: OrderKind::Mkt,
                tif: Tif::Day,
                order_ref: id.to_string(),
                account: "DU1".into(),
            },
        };
        fake.open.lock().unwrap().push((42, id.to_string(), state));
        fake.accounts.lock().unwrap().push("DU1".into());
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        on_connected(fake.as_ref(), &ctx, &mut booked).await.unwrap();
        handle(exec("e1", 42, ""), &ctx, &mut booked).await;
        assert!(fills(&mut rx).iter().any(|(o, k)| *o == id && matches!(k, ExecKind::Fill { .. })));
    }

    #[tokio::test]
    async fn a_replayed_execution_is_not_booked_twice() {
        let (fake, adapter) = rig();
        let id = Uuid::new_v4();
        fake.accounts.lock().unwrap().push("DU1".into());
        fake.executions.lock().unwrap().push(exec("e1", 42, &id.to_string()));
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        // Live, then the reconnect's catch-up replays it, then TWS echoes it again.
        handle(exec("e1", 42, &id.to_string()), &ctx, &mut booked).await;
        on_connected(fake.as_ref(), &ctx, &mut booked).await.unwrap();
        handle(exec("e1", 42, &id.to_string()), &ctx, &mut booked).await;
        let n = fills(&mut rx).iter().filter(|(_, k)| matches!(k, ExecKind::Fill { .. })).count();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn no_account_is_an_error_not_live() {
        let (fake, adapter) = rig();
        let (ctx, _rx, reg) = ctx(adapter);
        let err = on_connected(fake.as_ref(), &ctx, &mut Booked::default()).await.unwrap_err();
        assert_eq!(err, "IBKR returned no account");
        assert!(reg.snapshot().iter().all(|h| h.connected_since.is_none()));
    }

    #[tokio::test]
    async fn a_reject_and_a_cancel_reach_the_engine_once() {
        let (_, adapter) = rig();
        let id = Uuid::new_v4();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        let status = || TwsEvent::Status { order_id: 7, order_ref: id.to_string(), status: "Cancelled".into(), filled: 0.0, remaining: 1.0 };
        handle(status(), &ctx, &mut booked).await;
        handle(status(), &ctx, &mut booked).await;
        assert_eq!(fills(&mut rx), vec![(id, ExecKind::Canceled { reason: None })]);
        // A reject by id alone needs the id learned; unknown ids are skipped.
        handle(TwsEvent::Error { order_id: Some(9), code: 201, message: "no margin".into() }, &ctx, &mut booked).await;
        assert!(fills(&mut rx).is_empty());
        // One the adapter placed is known by its IBKR id.
        let placed = Uuid::new_v4();
        let req = crate::adapter::BrokerOrderRequest {
            order_id: placed.to_string(),
            symbol: "AAPL".into(),
            native_id: Some("265598".into()),
            quantity: 1.0,
            side: "buy".into(),
            order_type: "market".into(),
            time_in_force: "day".into(),
            limit_price: None,
            external_account_ref: "DU1".into(),
        };
        use crate::adapter::BrokerAdapter;
        let ibkr_id: i32 = ctx.adapter.submit_order(&req).await.unwrap().external_order_id.parse().unwrap();
        let reject = || TwsEvent::Error { order_id: Some(ibkr_id), code: 201, message: "no margin".into() };
        handle(reject(), &ctx, &mut booked).await;
        handle(reject(), &ctx, &mut booked).await;
        assert_eq!(fills(&mut rx), vec![(placed, ExecKind::Rejected { reason: "IB error 201: no margin".into() })]);
    }

    #[test]
    fn gateway_states_retry_except_a_failed_login() {
        assert!(matches!(on_gateway_state(&GatewayState::LoginFailed("bad password".into())), Next::Stop(_)));
        assert!(matches!(on_gateway_state(&GatewayState::AwaitingTwoFactor), Next::Retry(m) if m.contains("two-factor")));
        assert!(matches!(on_gateway_state(&GatewayState::Starting), Next::Retry(_)));
    }

    #[test]
    fn backoff_doubles_to_a_minute() {
        let mut d = FIRST_BACKOFF;
        let mut seen = vec![];
        for _ in 0..8 {
            seen.push(d.as_secs());
            d = next_backoff(d);
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }
}
