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
    /// Exec ids without their last `.NN` revision: a new id with a booked
    /// base is IBKR's correction of a fill, not another fill.
    exec_bases: HashSet<String>,
    canceled: HashSet<i32>,
    rejected: HashSet<i32>,
    /// OMS order of each exec id seen, so a commission report (which names
    /// only the exec id, and may come after the order is done) finds its order.
    exec_orders: HashMap<String, Uuid>,
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

/// `<base>.NN` -> `<base>`; IBKR revises a fill by bumping the last segment.
fn exec_base(exec_id: &str) -> &str {
    exec_id.rsplit_once('.').map_or(exec_id, |(base, _)| base)
}

impl Booked {
    /// Records `exec_id`; true when it revises a fill already booked.
    fn note_exec(&mut self, exec_id: &str) -> bool {
        let correction = !self.execs.contains(exec_id) && self.exec_bases.contains(exec_base(exec_id));
        self.execs.insert(exec_id.to_string());
        self.exec_bases.insert(exec_base(exec_id).to_string());
        correction
    }
}

/// Sends a commission report as the fee of its execution's order. The fee id
/// is the exec id, so the live report and a catch-up replay book once.
async fn send_commission(ctx: &Ctx, booked: &Booked, exec_id: String, amount: f64, currency: String) {
    // TWS sends f64::MAX when it has no commission figure yet; a zero is free.
    if amount == 0.0 || !amount.is_finite() || amount.abs() > 1e300 {
        return;
    }
    let Some(&order) = booked.exec_orders.get(&exec_id) else {
        return warn!(exec_id, amount, currency, "IBKR commission for an unknown execution; dropped");
    };
    let kind = ExecKind::Fee { fee_id: format!("{VENUE}-{exec_id}"), execution_id: Some(exec_id), amount, currency };
    ctx.engine.exec(ExecEvent { order_id: order, venue: VENUE.into(), actor: ACTOR, source: ExecSource::Session, kind }).await;
}

/// Books one client event, at most once per exec id / order.
pub async fn handle(ev: TwsEvent, ctx: &Ctx, booked: &mut Booked) {
    match ev {
        TwsEvent::Execution { exec_id, order_id, order_ref, qty, price, .. } => {
            let resolved = order_uuid(&ctx.adapter, order_id, &order_ref);
            if let Some(order) = resolved {
                booked.exec_orders.insert(exec_id.clone(), order);
            }
            if booked.execs.contains(&exec_id) {
                return;
            }
            let Some(order) = resolved else {
                return unknown_order(ctx, "execution", order_id).await;
            };
            if booked.note_exec(&exec_id) {
                // Booking it as a fill would double-count: recon settles the
                // order from the broker's corrected totals.
                warn!(exec_id, order_id, "IBKR corrected an execution; reconciling");
                return ctx.engine.reconcile(VENUE, ctx.env, ACTOR, ctx.adapter.clone()).await;
            }
            send(ctx, order, VenueReport::Fill { execution_id: exec_id, fill_qty: qty, fill_price: price, venue: VENUE.into() }).await;
        }
        TwsEvent::Commission { exec_id, amount, currency } => send_commission(ctx, booked, exec_id, amount, currency).await,
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
    let mut commissions = Vec::new();
    for ev in client.executions_today().await.map_err(|e| e.to_string())? {
        if let TwsEvent::Commission { exec_id, amount, currency } = ev {
            commissions.push((exec_id, amount, currency));
            continue;
        }
        if let TwsEvent::Execution { exec_id, order_id, order_ref, qty, price, .. } = ev {
            if let Some(order) = order_uuid(&ctx.adapter, order_id, &order_ref) {
                booked.exec_orders.insert(exec_id.clone(), order);
            }
            let fresh = !booked.execs.contains(&exec_id);
            if booked.note_exec(&exec_id) {
                // A correction: the reconcile after catch-up settles it.
                continue;
            }
            if fresh {
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
    // After the fills: the engine dedupes each fee on its id, so replaying
    // today's commissions books only the ones it missed.
    for (exec_id, amount, currency) in commissions {
        send_commission(ctx, booked, exec_id, amount, currency).await;
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
    /// Not up yet but nothing is wrong (starting, awaiting approval): connecting.
    Wait(String),
    /// Down: Docker missing or the session lost.
    Retry(String),
    Stop(String),
}

pub fn on_gateway_state(s: &GatewayState) -> Next {
    match s {
        GatewayState::LoginFailed(m) => Next::Stop(format!("IBKR login failed: {m}")),
        GatewayState::AwaitingTwoFactor => Next::Wait("waiting for two-factor approval on the IBKR mobile app".into()),
        GatewayState::ExistingSession => {
            Next::Wait("IBKR account is logged in elsewhere; the gateway is taking over the session".into())
        }
        GatewayState::Starting | GatewayState::Connected => Next::Wait("IB Gateway is starting".into()),
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

/// Publishes a connected client for one session and withdraws it when the
/// session ends — also when the supervisor task is aborted mid-session
/// (connection disabled or reloaded), so no stale client lingers in the
/// process-wide registry holding client id 1 open.
struct Published<'a> {
    code: &'a str,
    slot: &'a ClientSlot,
    client: Arc<dyn TwsClient>,
}

impl Drop for Published<'_> {
    fn drop(&mut self) {
        *self.slot.write().unwrap_or_else(|e| e.into_inner()) = None;
        super::set_live_client(self.code, None);
        self.client.shutdown();
    }
}

/// Runs one connected session with the client published to the adapter and the admin API.
pub async fn session(code: &str, slot: &ClientSlot, client: Arc<dyn TwsClient>, ctx: &Ctx, booked: &mut Booked) -> Result<(), String> {
    *slot.write().unwrap() = Some(client.clone());
    super::set_live_client(code, Some(client.clone()));
    let _published = Published { code, slot, client: client.clone() };
    run(client, ctx, booked).await
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
                        None => Next::Wait(format!("IB Gateway not reachable yet: {e}")),
                    }
                }
                Ok(client) => {
                    let r = session(&gw.code, &slot, Arc::new(client), &ctx, &mut booked).await;
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
            next => {
                if started.elapsed() >= STABLE_AFTER {
                    backoff = FIRST_BACKOFF;
                }
                match next {
                    Next::Wait(m) => {
                        info!(status = %m, "IBKR_TWS: checking again in {}s", backoff.as_secs());
                        ctx.health.set_connecting_because(m);
                    }
                    Next::Retry(m) | Next::Stop(m) => {
                        warn!(error = %m, "IBKR_TWS: retrying in {}s", backoff.as_secs());
                        ctx.health.set_down(m);
                    }
                }
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

    fn commission(exec_id: &str) -> TwsEvent {
        TwsEvent::Commission { exec_id: exec_id.into(), amount: 1.0, currency: "USD".into() }
    }

    fn fees(rx: &mut tokio::sync::mpsc::Receiver<GatewayMsg>) -> Vec<(Uuid, ExecKind)> {
        fills(rx).into_iter().filter(|(_, k)| matches!(k, ExecKind::Fee { .. })).collect()
    }

    #[tokio::test]
    async fn ibkr_commission_report_becomes_fee() {
        let (_, adapter) = rig();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let id = Uuid::new_v4();
        let mut booked = Booked::default();
        handle(exec("e1.01", 42, &id.to_string()), &ctx, &mut booked).await;
        handle(commission("e1.01"), &ctx, &mut booked).await;
        let kinds: Vec<_> = fills(&mut rx).into_iter().map(|(_, k)| k).collect();
        assert!(matches!(kinds[0], ExecKind::Fill { .. }), "fill first");
        assert_eq!(
            kinds[1],
            ExecKind::Fee { fee_id: "IBKR_TWS-e1.01".into(), execution_id: Some("e1.01".into()), amount: 1.0, currency: "USD".into() }
        );
    }

    #[tokio::test]
    async fn ibkr_commission_for_unknown_exec_is_dropped_with_warning() {
        let (_, adapter) = rig();
        let (ctx, mut rx, _reg) = ctx(adapter);
        handle(commission("nope"), &ctx, &mut Booked::default()).await;
        assert!(fills(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn ibkr_replayed_commission_has_the_same_fee_id() {
        let (fake, adapter) = rig();
        let id = Uuid::new_v4();
        fake.accounts.lock().unwrap().push("DU1".into());
        // Today's executions as TWS replays them: the commission may come first.
        fake.executions.lock().unwrap().push(commission("e1.01"));
        fake.executions.lock().unwrap().push(exec("e1.01", 42, &id.to_string()));
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        handle(exec("e1.01", 42, &id.to_string()), &ctx, &mut booked).await;
        handle(commission("e1.01"), &ctx, &mut booked).await;
        on_connected(fake.as_ref(), &ctx, &mut booked).await.unwrap();
        let ids: Vec<_> = fees(&mut rx)
            .into_iter()
            .map(|(o, k)| match k { ExecKind::Fee { fee_id, .. } => (o, fee_id), _ => unreachable!() })
            .collect();
        assert_eq!(ids, vec![(id, "IBKR_TWS-e1.01".to_string()), (id, "IBKR_TWS-e1.01".to_string())]);
    }

    #[tokio::test]
    async fn ibkr_zero_commission_sends_no_fee() {
        let (_, adapter) = rig();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let mut booked = Booked::default();
        handle(exec("e1", 42, &Uuid::new_v4().to_string()), &ctx, &mut booked).await;
        handle(TwsEvent::Commission { exec_id: "e1".into(), amount: 0.0, currency: "USD".into() }, &ctx, &mut booked).await;
        assert!(fees(&mut rx).is_empty());
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

    #[tokio::test]
    async fn a_corrected_execution_reconciles_instead_of_booking_another_fill() {
        let (_, adapter) = rig();
        let (ctx, mut rx, _reg) = ctx(adapter);
        let id = Uuid::new_v4();
        let mut booked = Booked::default();
        handle(exec("0000e0d5.6551a1b2.01.01", 42, &id.to_string()), &ctx, &mut booked).await;
        handle(exec("0000e0d5.6551a1b2.01.02", 42, &id.to_string()), &ctx, &mut booked).await;
        let (mut fills, mut reconciles) = (0, 0);
        while let Ok(m) = rx.try_recv() {
            match m {
                GatewayMsg::Exec(e) if matches!(e.kind, ExecKind::Fill { .. }) => fills += 1,
                GatewayMsg::Reconcile { .. } => reconciles += 1,
                _ => {}
            }
        }
        assert_eq!((fills, reconciles), (1, 1));
    }

    #[tokio::test]
    async fn aborting_a_session_withdraws_and_closes_its_client() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("DU1".into());
        let (ctx, _rx, _reg) = ctx(adapter);
        let slot: ClientSlot = Arc::new(std::sync::RwLock::new(None));
        let code = "IBKR_TWS-ABORTTEST";
        let task = {
            let (slot, client) = (slot.clone(), fake.clone() as Arc<dyn TwsClient>);
            tokio::spawn(async move { session(code, &slot, client, &ctx, &mut Booked::default()).await })
        };
        for _ in 0..100 {
            if super::super::live_client(code).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(super::super::live_client(code).is_some());
        task.abort();
        let _ = task.await;
        assert!(super::super::live_client(code).is_none());
        assert!(slot.read().unwrap().is_none());
        assert!(!fake.connected());
    }

    #[test]
    fn gateway_states_retry_except_a_failed_login() {
        assert!(matches!(on_gateway_state(&GatewayState::LoginFailed("bad password".into())), Next::Stop(_)));
        // Not up yet but nothing wrong: connecting, not down.
        assert!(matches!(on_gateway_state(&GatewayState::AwaitingTwoFactor), Next::Wait(m) if m.contains("two-factor")));
        assert!(matches!(on_gateway_state(&GatewayState::Starting), Next::Wait(_)));
        assert_eq!(
            on_gateway_state(&GatewayState::ExistingSession),
            Next::Wait("IBKR account is logged in elsewhere; the gateway is taking over the session".into())
        );
        assert!(matches!(on_gateway_state(&GatewayState::Disconnected), Next::Retry(_)));
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
