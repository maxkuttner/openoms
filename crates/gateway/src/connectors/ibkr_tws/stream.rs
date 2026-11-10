//! The IBKR_TWS execution stream and the gateway supervisor behind it.
//!
//! [`supervise`] keeps IB Gateway running and connected (Docker, then the TWS
//! API), swaps the client into the adapter's slot and runs [`run`] on it. [`run`]
//! turns the client's events into engine events; after every (re)connect it
//! checks the account, relearns order refs and replays every order's state as
//! cumulative / terminal totals (replay-safe in the engine, even after an OMS
//! restart); only executions arriving live in this process go as single fills,
//! once per exec id.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::contract::{BrokerOrderState, EngineHandle, ExecEvent, ExecKind, ExecSource, VenueReport};
use crate::stream_health::StreamHandle;

use super::adapter::{map_error, outcome, ClientSlot, ErrorClass, IbkrTwsAdapter};
use super::client::{IbapiClient, TwsClient, TwsEvent};
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

async fn send_state(ctx: &Ctx, order: Uuid, kind: ExecKind) {
    ctx.engine.exec(ExecEvent { order_id: order, venue: VENUE.into(), actor: ACTOR, source: ExecSource::Snapshot, kind }).await;
}

/// A fill or state for an order the stream can't name: shout, and let recon settle it.
async fn unknown_order(ctx: &Ctx, what: &str, order_id: i32) {
    let msg = format!("IBKR {what} for unknown order {order_id}; reconciling");
    error!(order_id, "{msg}");
    ctx.health.set_down(msg);
    ctx.engine.reconcile(VENUE, ctx.env, ACTOR, ctx.adapter.clone()).await;
}

/// Books one client event, at most once per exec id / order.
pub async fn handle(ev: TwsEvent, ctx: &Ctx, booked: &mut Booked) {
    match ev {
        TwsEvent::Execution { exec_id, order_id, order_ref, qty, price, .. } => {
            if booked.execs.contains(&exec_id) {
                return;
            }
            let Some(order) = order_uuid(&ctx.adapter, order_id, &order_ref) else {
                return unknown_order(ctx, "execution", order_id).await;
            };
            booked.execs.insert(exec_id.clone());
            send(ctx, order, VenueReport::Fill { execution_id: exec_id, fill_qty: qty, fill_price: price, venue: VENUE.into() }).await;
        }
        // The engine's fill carries no fee yet: recorded in the log only.
        TwsEvent::Commission { exec_id, amount, currency } => {
            info!(exec_id, amount, currency, "IBKR commission");
        }
        TwsEvent::Status { order_id, order_ref, status, filled, .. } => {
            if outcome(&status) == BrokerOrderOutcome::Canceled && booked.canceled.insert(order_id) {
                match order_uuid(&ctx.adapter, order_id, &order_ref) {
                    // Part-filled: its last execution may still be in flight, so
                    // recon closes it from the broker's final totals instead.
                    Some(_) if filled > 0.0 => ctx.engine.reconcile(VENUE, ctx.env, ACTOR, ctx.adapter.clone()).await,
                    Some(order) => send(ctx, order, VenueReport::Canceled { reason: None, venue: Some(VENUE.into()) }).await,
                    None => unknown_order(ctx, "cancel", order_id).await,
                }
            }
        }
        TwsEvent::Error { order_id: Some(id), code, message } if id > 0 => match map_error(code, &message) {
            ErrorClass::Reject if !ctx.adapter.rejected_on_submit(id) && booked.rejected.insert(id) => match order_uuid(&ctx.adapter, id, "") {
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

/// Replays every order's state from the broker's totals: `Terminal` for
/// finished orders, `Cumulative` for working ones. The engine books only what
/// it is missing from these, so a replay after an OMS restart books nothing
/// twice. Today's exec ids are marked booked so their live echo is skipped.
pub async fn catch_up(client: &dyn TwsClient, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    let open = client.open_orders().await.map_err(|e| e.to_string())?;
    let completed = client.completed_orders().await.map_err(|e| e.to_string())?;
    ctx.adapter.learn(&open);
    ctx.adapter.learn(&completed);
    // (qty, notional) per order from today's executions: the fallback total for
    // an order neither listing reports.
    let mut execs: HashMap<i32, (String, f64, f64)> = HashMap::new();
    for ev in client.executions_today().await.map_err(|e| e.to_string())? {
        if let TwsEvent::Execution { exec_id, order_id, order_ref, qty, price, .. } = ev {
            if booked.execs.insert(exec_id) {
                let e = execs.entry(order_id).or_insert((order_ref, 0.0, 0.0));
                e.1 += qty;
                e.2 += qty * price;
            }
        }
    }
    let mut unknown = false;
    for (id, order_ref, st) in &completed {
        execs.remove(id);
        let Some(order) = order_uuid(&ctx.adapter, *id, order_ref) else { unknown = true; continue };
        let o = outcome(&st.status);
        if o == BrokerOrderOutcome::Canceled {
            booked.canceled.insert(*id);
        }
        if o == BrokerOrderOutcome::Rejected {
            booked.rejected.insert(*id);
        }
        let state = BrokerOrderState { outcome: o, executed_qty: st.filled, avg_px: st.avg_fill_price };
        send_state(ctx, order, ExecKind::Terminal(state)).await;
    }
    for (id, order_ref, st) in &open {
        execs.remove(id);
        let Some(order) = order_uuid(&ctx.adapter, *id, order_ref) else { unknown = true; continue };
        if st.filled > 0.0 {
            send_state(ctx, order, ExecKind::Cumulative { cum_qty: st.filled, avg_px: st.avg_fill_price }).await;
        }
    }
    for (id, (order_ref, qty, notional)) in execs {
        let Some(order) = order_uuid(&ctx.adapter, id, &order_ref) else { unknown = true; continue };
        send_state(ctx, order, ExecKind::Cumulative { cum_qty: qty, avg_px: notional / qty }).await;
    }
    if unknown {
        warn!("IBKR reports orders the OMS can't name; the reconcile settles them");
    }
    Ok(())
}

/// After a (re)connect: account check, catch-up, then live and a reconcile.
pub async fn on_connected(client: &dyn TwsClient, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    let accounts = client.managed_accounts().await.map_err(|e| e.to_string())?;
    if accounts.is_empty() {
        return Err("IBKR returned no account".into());
    }
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
        GatewayState::Unavailable(m) => Next::Retry(m.clone()),
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
                    super::set_live_client(&gw.code, Some(client.clone()));
                    let r = run(client, &ctx, &mut booked).await;
                    *slot.write().unwrap() = None;
                    super::set_live_client(&gw.code, None);
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
    use super::super::client::{OrderKind, Side, Tif, TwsOrder, TwsOrderState};
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

    fn order_state(id: i32, order_ref: &str, status: &str, filled: f64, avg: f64) -> (i32, String, TwsOrderState) {
        let st = TwsOrderState {
            status: status.into(),
            filled,
            remaining: 10.0 - filled,
            avg_fill_price: avg,
            symbol: "AAPL".into(),
            order: TwsOrder {
                order_id: id,
                con_id: 265598,
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Buy,
                qty: 10.0,
                order_type: OrderKind::Mkt,
                tif: Tif::Day,
                order_ref: order_ref.into(),
                account: "DU1".into(),
            },
        };
        (id, order_ref.into(), st)
    }

    fn msgs(rx: &mut tokio::sync::mpsc::Receiver<GatewayMsg>) -> (Vec<ExecEvent>, usize) {
        let (mut execs, mut recons) = (Vec::new(), 0);
        while let Ok(m) = rx.try_recv() {
            match m {
                GatewayMsg::Exec(e) => execs.push(e),
                GatewayMsg::Reconcile { .. } => recons += 1,
            }
        }
        (execs, recons)
    }

    /// After an OMS restart nothing is in `Booked`, and the engine has already
    /// booked these fills: the replay must be in replay-safe totals, not fills.
    #[tokio::test]
    async fn a_restart_replays_totals_not_fills() {
        let (fake, adapter) = rig();
        let (working, done) = (Uuid::new_v4(), Uuid::new_v4());
        fake.accounts.lock().unwrap().push("DU1".into());
        fake.open.lock().unwrap().push(order_state(42, &working.to_string(), "Submitted", 5.0, 190.2));
        fake.completed.lock().unwrap().push(order_state(43, &done.to_string(), "Filled", 10.0, 191.0));
        for (e, id, r) in [("e1", 42, working), ("e2", 42, working), ("e3", 43, done)] {
            fake.executions.lock().unwrap().push(exec(e, id, &r.to_string()));
        }
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        on_connected(fake.as_ref(), &ctx, &mut booked).await.unwrap();
        // TWS echoes the replayed executions onto the live stream too.
        handle(exec("e1", 42, &working.to_string()), &ctx, &mut booked).await;
        let (execs, _) = msgs(&mut rx);
        let kinds: Vec<(Uuid, ExecKind)> = execs.into_iter().map(|e| (e.order_id, e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (done, ExecKind::Terminal(BrokerOrderState { outcome: BrokerOrderOutcome::Filled, executed_qty: 10.0, avg_px: 191.0 })),
                (working, ExecKind::Cumulative { cum_qty: 5.0, avg_px: 190.2 }),
            ]
        );
    }

    #[tokio::test]
    async fn executions_for_an_order_neither_list_reports_replay_as_a_total() {
        let (fake, adapter) = rig();
        let id = Uuid::new_v4();
        fake.accounts.lock().unwrap().push("DU1".into());
        fake.executions.lock().unwrap().push(exec("e1", 42, &id.to_string()));
        fake.executions.lock().unwrap().push(TwsEvent::Execution {
            exec_id: "e2".into(), order_id: 42, order_ref: id.to_string(), qty: 1.0, price: 200.0, time: Utc::now(),
        });
        let (ctx, mut rx, _reg) = ctx(adapter);
        on_connected(fake.as_ref(), &ctx, &mut Booked::default()).await.unwrap();
        let (execs, _) = msgs(&mut rx);
        let want = ExecKind::Cumulative { cum_qty: 4.0, avg_px: (3.0 * 190.25 + 200.0) / 4.0 };
        assert_eq!(execs.into_iter().map(|e| e.kind).collect::<Vec<_>>(), vec![want]);
    }

    #[tokio::test]
    async fn a_fill_for_an_unknown_order_shows_on_health_and_reconciles() {
        let (_, adapter) = rig();
        let (ctx, mut rx, reg) = ctx(adapter);
        handle(exec("e1", 77, ""), &ctx, &mut Booked::default()).await;
        let (execs, recons) = msgs(&mut rx);
        assert!(execs.is_empty());
        assert_eq!(recons, 1);
        let err = reg.snapshot()[0].last_error.clone().unwrap();
        assert!(err.contains("unknown order 77"), "{err}");
    }

    #[tokio::test]
    async fn a_part_filled_cancel_is_left_to_recon() {
        let (_, adapter) = rig();
        let id = Uuid::new_v4();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let ev = TwsEvent::Status { order_id: 7, order_ref: id.to_string(), status: "Cancelled".into(), filled: 2.0, remaining: 8.0 };
        handle(ev, &ctx, &mut Booked::default()).await;
        let (execs, recons) = msgs(&mut rx);
        assert!(execs.is_empty());
        assert_eq!(recons, 1);
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
        assert_eq!(
            on_gateway_state(&GatewayState::Unavailable("Docker is required for IBKR_TWS but is not running".into())),
            Next::Retry("Docker is required for IBKR_TWS but is not running".into())
        );
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
