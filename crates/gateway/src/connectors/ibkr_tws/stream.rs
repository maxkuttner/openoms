//! The IBKR_TWS execution stream and the gateway supervisor behind it.
//!
//! [`supervise`] keeps IB Gateway running and connected (Docker, then the TWS
//! API; or straight to the address of a gateway run as its own service), swaps the client into the adapter's slot and runs [`run`] on it. [`run`]
//! turns the client's events into venue events for the reconciler: every
//! execution it is shown is a fill carrying its IBKR account number, every
//! `openOrder`/`orderStatus` an order state, every commission report a fee.
//! What it is shown: the OMS's own orders (this client id), and, as client 0
//! (`reqAutoOpenOrders(true)`), orders typed into TWS. Other API clients'
//! orders and executions stream only to the client whose id is IB Gateway's
//! "Master API client ID" setting, which the OMS does not configure. After
//! each (re)connect it sends `Connected`, so the reconciler pulls what the
//! stream missed (`reqExecutions`) and the open orders.
//!
//! `VenueEvent::Connected` doubles as the stream's "pull now" request: the
//! reconciler answers every `Connected` with a pull, which is what a
//! corrected execution, a 1101/1102 reconnect and a lagged event channel need.
//!
//! Fills go out under the exec id's base (`<base>` of `<base>.NN`), the same
//! id the pull uses: IBKR corrects a fill by bumping `.NN`, and a correction
//! must not book as a second fill. The stream emits nothing for one and asks
//! for a pull instead, which dedupes on the base.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::stream_health::StreamHandle;
use crate::venue_event::{VenueEvent, VenueOrderStatus, VenueOrderUpdate};

use super::adapter::{commission_fee, correction_warning, exec_base, fill_from_exec, map_error, order_update, ClientSlot, ErrorClass, IbkrTwsAdapter};
use super::client::{IbapiClient, SecType, TwsClient, TwsEvent};
use super::gateway::{classify_logs, container_name, ensure_running, GatewayDocker, GatewayState};
use super::IbkrTwsCredentials;

/// How long to wait for the client's own reconnect before restarting from Docker.
const RECONNECT_WAIT: Duration = Duration::from_secs(120);
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A session that lasted this long was healthy: the next failure retries quickly.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// How a live session checks IB Gateway is still there: a server-time request
/// `every`, which must answer within `timeout` or the session ends.
#[derive(Clone, Copy, Debug)]
pub struct Heartbeat {
    pub every: Duration,
    pub timeout: Duration,
}

/// The live session's heartbeat: a ping every 30s, answered within 10s.
pub const HEARTBEAT: Heartbeat = Heartbeat { every: Duration::from_secs(30), timeout: Duration::from_secs(10) };

/// What this stream has seen, kept across reconnects.
#[derive(Default)]
pub struct Seen {
    /// Exec base -> (full exec id, qty, price) of the revision first sent.
    execs: HashMap<String, (String, f64, f64)>,
    /// Full exec id -> IBKR account, for the commission report that names only the exec id.
    exec_accounts: HashMap<String, String>,
    /// Last state per IBKR order id, so a reject (which names only the id) has an order.
    orders: HashMap<i32, VenueOrderUpdate>,
    rejected: HashSet<i32>,
    /// Exec ids of combo (BAG) executions: not fills, so their commissions are not fees.
    bag_execs: HashSet<String>,
}

/// Everything one session needs besides the client.
pub struct Ctx {
    pub adapter: Arc<IbkrTwsAdapter>,
    pub health: StreamHandle,
    pub tx: mpsc::Sender<VenueEvent>,
}

/// The venue events one client event stands for. `Connected` asks the
/// reconciler for a pull (a corrected execution).
pub fn handle(ev: TwsEvent, adapter: &IbkrTwsAdapter, seen: &mut Seen) -> Vec<VenueEvent> {
    match ev {
        TwsEvent::Execution(e) if e.sec_type == SecType::Bag => {
            // The combo's own row; its legs arrive as ordinary executions.
            debug!(exec_id = %e.exec_id, "IBKR combo execution is not a fill");
            seen.bag_execs.insert(e.exec_id.clone());
            vec![]
        }
        TwsEvent::Execution(e) => {
            let client = adapter.client_ref(e.order_id, &e.order_ref);
            let fill = match fill_from_exec(&e, client) {
                Ok(f) => f,
                Err(missing) => {
                    warn!(exec_id = %e.exec_id, missing, "IBKR execution skipped: field missing");
                    return vec![];
                }
            };
            seen.exec_accounts.insert(e.exec_id.clone(), e.account.clone());
            let base = exec_base(&e.exec_id).to_string();
            match seen.execs.get(&base) {
                None => {
                    seen.execs.insert(base, (e.exec_id.clone(), e.qty, e.price));
                    vec![VenueEvent::Fill(fill)]
                }
                Some((id, _, _)) if *id == e.exec_id => vec![],
                Some((_, qty, price)) => {
                    if let Some(w) = correction_warning(&base, (*qty, *price), (e.qty, e.price)) {
                        warn!(exec_id = %e.exec_id, "{w}");
                    }
                    warn!(exec_id = %e.exec_id, "IBKR corrected an execution; no new fill, requesting a pull");
                    // `Connected` = "pull now" for the reconciler (see module doc).
                    vec![VenueEvent::Connected]
                }
            }
        }
        TwsEvent::Commission { exec_id, .. } if seen.bag_execs.contains(&exec_id) => vec![],
        TwsEvent::Commission { exec_id, amount, currency } => {
            let Some((amount, currency)) = commission_fee(amount, currency) else { return vec![] };
            let Some(account_ref) = seen.exec_accounts.get(&exec_id).cloned() else {
                warn!(exec_id, amount, currency, "IBKR commission for an execution not seen; the pull carries it");
                return vec![];
            };
            vec![VenueEvent::Fee { exec_id: exec_base(&exec_id).to_string(), account_ref, amount, currency }]
        }
        TwsEvent::Order { order_id, order_ref, state } => {
            let mut u = order_update(order_id, &state);
            u.client_order_id = adapter.client_ref(order_id, &order_ref);
            seen.orders.insert(order_id, u.clone());
            vec![VenueEvent::Order(u)]
        }
        TwsEvent::Error { order_id: Some(id), code, message } if id > 0 => match map_error(code, &message) {
            ErrorClass::Reject if !adapter.rejected_on_submit(id) && seen.rejected.insert(id) => {
                match seen.orders.get(&id).cloned().or_else(|| adapter.placed(id)) {
                    Some(mut u) => {
                        u.status = VenueOrderStatus::Rejected { reason: format!("IB error {code}: {message}") };
                        vec![VenueEvent::Order(u)]
                    }
                    None => {
                        warn!(order_id = id, code, message, "IBKR reject for an order this stream has not seen");
                        vec![]
                    }
                }
            }
            _ => {
                info!(order_id = id, code, message, "IBKR order notice");
                vec![]
            }
        },
        TwsEvent::Error { code, message, .. } => {
            info!(code, message, "IBKR notice");
            vec![]
        }
        TwsEvent::Disconnected => vec![],
    }
}

async fn send(ctx: &Ctx, events: Vec<VenueEvent>) -> Result<(), String> {
    for e in events {
        ctx.tx.send(e).await.map_err(|_| "IBKR venue event sink closed".to_string())?;
    }
    Ok(())
}

/// After a (re)connect: account check, order refs relearned, then live and a pull.
pub async fn on_connected(client: &dyn TwsClient, ctx: &Ctx) -> Result<(), String> {
    let accounts = client.managed_accounts().await.map_err(|e| e.to_string())?;
    if accounts.is_empty() {
        return Err("IBKR returned no account".into());
    }
    ctx.adapter.learn(&client.open_orders().await.map_err(|e| e.to_string())?);
    ctx.adapter.learn(&client.completed_orders().await.map_err(|e| e.to_string())?);
    ctx.health.set_live();
    info!(accounts = ?accounts, "IBKR execution stream live");
    send(ctx, vec![VenueEvent::Connected]).await
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

/// One heartbeat: IB Gateway must answer a server-time request within `hb.timeout`.
async fn ping(client: &dyn TwsClient, hb: &Heartbeat) -> Result<(), String> {
    match tokio::time::timeout(hb.timeout, client.ping()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("IB Gateway did not answer the heartbeat: {e}")),
        Err(_) => Err(format!("IB Gateway did not answer the heartbeat within {}s", hb.timeout.as_secs())),
    }
}

/// [`run_with`] on the production [`HEARTBEAT`].
pub async fn run(client: Arc<dyn TwsClient>, ctx: &Ctx, seen: &mut Seen) -> Result<(), String> {
    run_with(client, ctx, seen, &HEARTBEAT).await
}

/// Runs one connected client until it is lost for good. Rides out the
/// client's own reconnects, asking for a pull after each, and pings IB Gateway
/// every `hb.every`: an answer is a health check, a failure ends the session.
pub async fn run_with(client: Arc<dyn TwsClient>, ctx: &Ctx, seen: &mut Seen, hb: &Heartbeat) -> Result<(), String> {
    // Subscribe first, so nothing between the connect and the loop is missed.
    let mut events = client.events();
    on_connected(client.as_ref(), ctx).await?;
    let mut beat = tokio::time::interval(hb.every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await; // the first tick is immediate; on_connected just checked
    // IB Gateway reported (1100) it lost IBKR: the stream is already down and
    // the gateway reconnects on its own, so a failed ping meanwhile is expected.
    let mut ibkr_lost = false;
    loop {
        let ev = tokio::select! {
            ev = events.recv() => ev,
            _ = beat.tick() => {
                match ping(client.as_ref(), hb).await {
                    Ok(()) => ctx.health.record_check(),
                    Err(e) if ibkr_lost => debug!(error = %e, "IB Gateway heartbeat failed while it is reconnecting to IBKR"),
                    Err(e) => return Err(e),
                }
                continue;
            }
        };
        match ev {
            Ok(TwsEvent::Disconnected) => {
                // ibapi is reconnecting on its own: an attempt is in progress.
                ctx.health.set_connecting_because("IB Gateway connection lost; reconnecting");
                send(ctx, vec![VenueEvent::Disconnected]).await?;
                warn!("IB Gateway connection lost; waiting for it to come back");
                if !wait_reconnect(client.as_ref(), RECONNECT_WAIT).await {
                    return Err("IB Gateway connection lost".into());
                }
                ibkr_lost = false;
                on_connected(client.as_ref(), ctx).await?;
                beat.reset();
            }
            Ok(TwsEvent::Error { order_id: None, code: 1100, .. }) => {
                ibkr_lost = true;
                ctx.health.set_down("IB Gateway lost its connection to IBKR");
            }
            Ok(TwsEvent::Error { order_id: None, code: code @ (1101 | 1102), .. }) => {
                info!(code, "IB Gateway reconnected to IBKR; pulling");
                ibkr_lost = false;
                ctx.health.set_live();
                send(ctx, vec![VenueEvent::Connected]).await?;
            }
            Ok(ev) => {
                let out = handle(ev, &ctx.adapter, seen);
                if out.is_empty() {
                    ctx.health.record_check();
                } else {
                    ctx.health.record_event();
                }
                send(ctx, out).await?;
            }
            Err(RecvError::Lagged(n)) => {
                warn!(missed = n, "IBKR event stream lagged; pulling");
                send(ctx, vec![VenueEvent::Connected]).await?;
            }
            Err(RecvError::Closed) => return Err("IBKR event stream closed".into()),
        }
    }
}

/// What the supervisor does with a gateway state it can't connect past.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// The gateway reports it is logging in (starting, awaiting two-factor
    /// approval, taking over a session): still connecting.
    Wait(String),
    /// The attempt failed (unreachable, Docker missing, the session lost):
    /// down until the next attempt.
    Retry(String),
    /// Will not retry until the user changes the credentials or configuration.
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

/// Where the gateway lives and how to log in; `creds.gateway` set = an
/// external gateway, and `docker` is never called.
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
/// process-wide registry holding its client id open.
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
pub async fn session(
    code: &str,
    slot: &ClientSlot,
    client: Arc<dyn TwsClient>,
    ctx: &Ctx,
    seen: &mut Seen,
    hb: &Heartbeat,
) -> Result<(), String> {
    *slot.write().unwrap() = Some(client.clone());
    super::set_live_client(code, Some(client.clone()));
    let _published = Published { code, slot, client: client.clone() };
    run_with(client, ctx, seen, hb).await
}

/// One connect attempt: the connected client, or what the supervisor does
/// instead. An external gateway (`gateway` set) is connected to directly, with
/// no Docker call: unreachable is retried, never read as a failed login
/// (its logs are not ours to read). A managed one is started or adopted first
/// and reached on loopback; a refused connect reads its logs for why.
pub async fn attempt<C, E: std::fmt::Display, F: std::future::Future<Output = Result<C, E>>>(
    gw: &Gateway,
    connect: impl FnOnce(String) -> F,
) -> Result<C, Next> {
    attempt_within(gw, EXTERNAL_CONNECT_TIMEOUT, connect).await
}

/// How long a connect to an external gateway may take before it counts as unreachable.
const EXTERNAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// [`attempt`] with the external connect bounded by `limit`.
pub async fn attempt_within<C, E: std::fmt::Display, F: std::future::Future<Output = Result<C, E>>>(
    gw: &Gateway,
    limit: Duration,
    connect: impl FnOnce(String) -> F,
) -> Result<C, Next> {
    match gw.creds.external() {
        Err(why) => return Err(Next::Stop(format!("IBKR_TWS gateway address: {why}"))),
        Ok(Some((host, port))) => {
            let unreachable = |e: &dyn std::fmt::Display| Next::Retry(format!("IB Gateway at {host}:{port} not reachable: {e}"));
            return match tokio::time::timeout(limit, connect(format!("{host}:{port}"))).await {
                Ok(r) => r.map_err(|e| unreachable(&e)),
                Err(_) => Err(unreachable(&"timed out")),
            };
        }
        Ok(None) => {}
    }
    let port = ensure_running(gw.docker.clone(), &gw.code, gw.env, &gw.creds, &gw.state_dir)
        .await
        .map_err(|state| on_gateway_state(&state))?;
    match connect(format!("127.0.0.1:{port}")).await {
        Ok(client) => Ok(client),
        Err(e) => {
            let docker = gw.docker.clone();
            let name = container_name(&gw.code);
            let tail = tokio::task::spawn_blocking(move || docker.logs_tail(&name, 200)).await.unwrap_or_default();
            Err(match classify_logs(&tail) {
                Some(state) => on_gateway_state(&state),
                None => Next::Retry(format!("IB Gateway not reachable yet: {e}")),
            })
        }
    }
}

/// For an external gateway: what to do instead of trading when the accounts it
/// is logged into do not belong to the connection's environment (IBKR paper
/// account ids start with "D": DU…, DF…). The OMS did not log it in, so the
/// address alone says nothing about paper or live. `None` = go ahead; always
/// for a managed gateway, which the OMS logged in with this environment's mode.
pub async fn external_mismatch(gw: &Gateway, client: &dyn TwsClient) -> Option<Next> {
    let Ok(Some((host, port))) = gw.creds.external() else { return None };
    let addr = format!("{host}:{port}");
    let accounts = match client.managed_accounts().await {
        Ok(a) => a,
        Err(e) => return Some(Next::Retry(format!("IB Gateway at {addr}: could not read its accounts: {e}"))),
    };
    if accounts.is_empty() {
        return Some(Next::Retry(format!("IB Gateway at {addr} reported no accounts")));
    }
    account_mismatch(&format!("IB Gateway at {addr}"), gw.env, &accounts).map(Next::Stop)
}

/// Whether `accounts` (non-empty) are paper accounts: IBKR paper ids start with "D".
pub fn paper_accounts(accounts: &[String]) -> bool {
    accounts.iter().all(|a| a.starts_with('D'))
}

/// Why a gateway (`who`, e.g. "IB Gateway at ib-gateway:4004") logged into
/// `accounts` must not trade for a connection in `env`; `None` = they match.
pub fn account_mismatch(who: &str, env: &str, accounts: &[String]) -> Option<String> {
    let paper = paper_accounts(accounts);
    if env.eq_ignore_ascii_case("live") {
        paper.then(|| format!("{who} is logged into a paper account but this connection is LIVE"))
    } else {
        (!paper).then(|| format!("{who} is logged into a live account but this connection is PAPER"))
    }
}

/// Keeps IB Gateway up and the stream running, backing off 1s -> 60s. A failed
/// login stops it: retrying a bad password risks locking the account.
pub async fn supervise(gw: Gateway, slot: ClientSlot, ctx: Ctx) {
    let client_id = gw.creds.master_client_id;
    supervise_with(gw, slot, ctx, HEARTBEAT, move |addr: String| async move {
        IbapiClient::connect(&addr, client_id).await.map(|c| Arc::new(c) as Arc<dyn TwsClient>)
    })
    .await
}

/// [`supervise`] with the connect and the heartbeat injected. What the health
/// shows: `connecting` while an attempt runs (or the gateway reports it is
/// logging in), `down` with the reason and the retry time between attempts,
/// `stopped` when it will not retry.
pub async fn supervise_with<C, E, F>(gw: Gateway, slot: ClientSlot, ctx: Ctx, hb: Heartbeat, connect: C)
where
    C: Fn(String) -> F,
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<Arc<dyn TwsClient>, E>>,
{
    let mut backoff = FIRST_BACKOFF;
    let mut seen = Seen::default();
    ctx.health.set_heartbeat_secs(hb.every.as_secs());
    loop {
        ctx.health.set_connecting();
        let started = Instant::now();
        let next = match attempt(&gw, &connect).await {
            Err(next) => next,
            Ok(client) => match external_mismatch(&gw, client.as_ref()).await {
                Some(next) => {
                    client.shutdown();
                    next
                }
                None => {
                    let r = session(&gw.code, &slot, client, &ctx, &mut seen, &hb).await;
                    Next::Retry(r.err().unwrap_or_else(|| "IBKR session ended".into()))
                }
            },
        };
        if started.elapsed() >= STABLE_AFTER {
            backoff = FIRST_BACKOFF;
        }
        match next {
            Next::Stop(m) => {
                warn!(error = %m, "IBKR_TWS stream stopped");
                ctx.health.set_stopped(m);
                return;
            }
            Next::Wait(m) => {
                info!(status = %m, "IBKR_TWS: checking again in {}s", backoff.as_secs());
                ctx.health.set_connecting_because(m);
            }
            Next::Retry(m) => {
                warn!(error = %m, "IBKR_TWS: retrying in {}s", backoff.as_secs());
                ctx.health.set_down_retrying(m, backoff);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use uuid::Uuid;

    use super::super::adapter::tests::rig;
    use super::super::client::{OrderKind, Side, Tif, TwsExecution, TwsOrder, TwsOrderState};
    use super::super::client::FakeTws;
    use super::super::gateway::{ContainerInfo, RunSpec};
    use super::*;
    use crate::stream_health::{StreamHealthRegistry, StreamKind, StreamState};
    use crate::venue_event::VenueFill;

    fn ctx(adapter: Arc<IbkrTwsAdapter>) -> (Ctx, mpsc::Receiver<VenueEvent>, StreamHealthRegistry) {
        let reg = StreamHealthRegistry::new();
        let health = reg.handle("IBKR_TWS", "PAPER", StreamKind::Execution);
        let (tx, rx) = mpsc::channel(64);
        (Ctx { adapter, health, tx }, rx, reg)
    }

    pub(crate) fn execution(exec_id: &str, order_id: i32, order_ref: &str, account: &str) -> TwsExecution {
        TwsExecution {
            exec_id: exec_id.into(),
            order_id,
            perm_id: 900_000 + order_id as i64,
            order_ref: order_ref.into(),
            account: account.into(),
            symbol: "AAPL".into(),
            local_symbol: "AAPL".into(),
            con_id: 265598,
            sec_type: SecType::Stk,
            buy: true,
            qty: 3.0,
            price: 190.25,
            time: Utc::now(),
        }
    }

    fn exec(exec_id: &str, order_id: i32, order_ref: &str) -> TwsEvent {
        TwsEvent::Execution(execution(exec_id, order_id, order_ref, "U1"))
    }

    fn fills(events: &[VenueEvent]) -> Vec<VenueFill> {
        events.iter().filter_map(|e| if let VenueEvent::Fill(f) = e { Some(f.clone()) } else { None }).collect()
    }

    #[test]
    fn a_combo_execution_and_its_commission_produce_no_events() {
        let (_fake, adapter) = rig();
        let mut seen = Seen::default();
        let mut bag = execution("b1.01", 7, "", "U1");
        bag.sec_type = SecType::Bag;
        bag.symbol = "ES".into();
        bag.local_symbol = "ES-ZN".into();
        assert!(handle(TwsEvent::Execution(bag), &adapter, &mut seen).is_empty());
        let fee = TwsEvent::Commission { exec_id: "b1.01".into(), amount: 2.0, currency: "USD".into() };
        assert!(handle(fee, &adapter, &mut seen).is_empty());
        // A leg is still a fill.
        assert_eq!(handle(exec("l1.01", 8, ""), &adapter, &mut seen).len(), 1);
    }

    #[tokio::test]
    async fn manual_tws_execution_is_a_venue_fill_not_a_fault() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("U1".into());
        let (ctx, mut rx, reg) = ctx(adapter);
        let client: Arc<dyn TwsClient> = fake.clone();
        let task = tokio::spawn(async move { run(client, &ctx, &mut Seen::default()).await });
        assert_eq!(rx.recv().await, Some(VenueEvent::Connected));
        // Typed into TWS: no order ref, a negative (auto-bound) order id.
        fake.events_tx.send(exec("m1.01", -5, "")).unwrap();
        let Some(VenueEvent::Fill(f)) = rx.recv().await else { panic!("a fill") };
        assert_eq!((f.exec_id.as_str(), f.client_order_id.clone(), f.venue_order_id.as_str()), ("m1", None, "899995"));
        let h = &reg.snapshot()[0];
        assert!(h.last_error.is_none() && h.connected_since.is_some(), "health stays up");
        task.abort();
    }

    #[tokio::test]
    async fn execution_carries_account_ref() {
        let (_, adapter) = rig();
        let id = Uuid::new_v4();
        let mut seen = Seen::default();
        let a = handle(TwsEvent::Execution(execution("a.01", 42, &id.to_string(), "U1")), &adapter, &mut seen);
        let b = handle(TwsEvent::Execution(execution("b.01", 43, "", "U2")), &adapter, &mut seen);
        let (a, b) = (&fills(&a)[0], &fills(&b)[0]);
        assert_eq!((a.account_ref.as_str(), b.account_ref.as_str()), ("U1", "U2"));
        // An OMS order keeps matching by its UUID order ref.
        assert_eq!(a.client_order_id.as_deref(), Some(id.to_string().as_str()));
        assert_eq!((a.symbol.as_str(), a.buy, a.qty, a.price), ("AAPL", true, 3.0, 190.25));
    }

    #[tokio::test]
    async fn a_fill_without_a_ref_maps_by_the_learned_order_id() {
        let (_, adapter) = rig();
        let id = Uuid::new_v4();
        adapter.learn(&[order_state(42, &id.to_string(), "Submitted", 0.0, 0.0)]);
        let out = handle(exec("e1", 42, ""), &adapter, &mut Seen::default());
        assert_eq!(fills(&out)[0].client_order_id.as_deref(), Some(id.to_string().as_str()));
    }

    #[tokio::test]
    async fn commission_maps_to_fee_event() {
        let (_, adapter) = rig();
        let mut seen = Seen::default();
        handle(exec("e1.01", 42, ""), &adapter, &mut seen);
        let fee = TwsEvent::Commission { exec_id: "e1.01".into(), amount: 1.25, currency: "USD".into() };
        assert_eq!(
            handle(fee, &adapter, &mut seen),
            vec![VenueEvent::Fee { exec_id: "e1".into(), account_ref: "U1".into(), amount: 1.25, currency: "USD".into() }]
        );
        // No figure yet (f64::MAX), free, or for an execution never seen: nothing.
        for (id, amount) in [("e1.01", f64::MAX), ("e1.01", 0.0), ("nope", 1.0)] {
            let c = TwsEvent::Commission { exec_id: id.into(), amount, currency: "USD".into() };
            assert!(handle(c, &adapter, &mut seen).is_empty());
        }
    }

    #[tokio::test]
    async fn corrected_execution_triggers_pull() {
        let (_, adapter) = rig();
        let mut seen = Seen::default();
        let first = handle(exec("0000e0d5.6551a1b2.01.01", 42, ""), &adapter, &mut seen);
        assert_eq!(fills(&first)[0].exec_id, "0000e0d5.6551a1b2.01");
        // The echo of the same revision: nothing.
        assert!(handle(exec("0000e0d5.6551a1b2.01.01", 42, ""), &adapter, &mut seen).is_empty());
        let corrected = handle(exec("0000e0d5.6551a1b2.01.02", 42, ""), &adapter, &mut seen);
        assert_eq!(corrected, vec![VenueEvent::Connected]);
    }

    #[tokio::test]
    async fn a_malformed_execution_is_skipped() {
        let (_, adapter) = rig();
        let mut e = execution("e1", 42, "", "");
        assert!(handle(TwsEvent::Execution(e.clone()), &adapter, &mut Seen::default()).is_empty());
        e.account = "U1".into();
        e.qty = 0.0;
        assert!(handle(TwsEvent::Execution(e), &adapter, &mut Seen::default()).is_empty());
    }

    fn order_state(id: i32, order_ref: &str, status: &str, filled: f64, avg: f64) -> (i32, String, TwsOrderState) {
        let st = TwsOrderState {
            status: status.into(),
            filled,
            remaining: 10.0 - filled,
            avg_fill_price: avg,
            symbol: "AAPL".into(),
            perm_id: 777,
            order: TwsOrder {
                order_id: id,
                con_id: 265598,
                symbol: "AAPL".into(),
                exchange: "SMART".into(),
                primary_exchange: "NASDAQ".into(),
                currency: "USD".into(),
                action: Side::Buy,
                qty: 10.0,
                order_type: OrderKind::Lmt(190.0),
                tif: Tif::Day,
                order_ref: order_ref.into(),
                account: "U2".into(),
                sec_type: SecType::Stk,
                legs: vec![],
            },
        };
        (id, order_ref.into(), st)
    }

    #[tokio::test]
    async fn an_order_state_is_a_venue_order_update_by_perm_id() {
        let (_, adapter) = rig();
        let id = Uuid::new_v4();
        let (oid, r, st) = order_state(7, &id.to_string(), "Cancelled", 2.0, 190.0);
        let out = handle(TwsEvent::Order { order_id: oid, order_ref: r, state: st }, &adapter, &mut Seen::default());
        let [VenueEvent::Order(u)] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!((u.venue_order_id.as_str(), u.account_ref.as_str(), u.cum_qty), ("777", "U2", 2.0));
        assert_eq!(u.client_order_id.as_deref(), Some(id.to_string().as_str()));
        assert_eq!((u.status.clone(), u.order_type.as_str(), u.limit_px), (VenueOrderStatus::Canceled, "limit", Some(190.0)));
    }

    #[tokio::test]
    async fn a_reject_reaches_the_reconciler_once() {
        let (_, adapter) = rig();
        let mut seen = Seen::default();
        let (oid, r, st) = order_state(9, "", "Submitted", 0.0, 0.0);
        handle(TwsEvent::Order { order_id: oid, order_ref: r, state: st }, &adapter, &mut seen);
        let reject = || TwsEvent::Error { order_id: Some(9), code: 201, message: "no margin".into() };
        let out = handle(reject(), &adapter, &mut seen);
        assert!(matches!(&out[..], [VenueEvent::Order(u)] if u.status == VenueOrderStatus::Rejected { reason: "IB error 201: no margin".into() }));
        assert!(handle(reject(), &adapter, &mut seen).is_empty());
        // Never seen: warned, nothing sent.
        assert!(handle(TwsEvent::Error { order_id: Some(10), code: 201, message: "x".into() }, &adapter, &mut seen).is_empty());
    }

    #[tokio::test]
    async fn no_account_is_an_error_not_live() {
        let (fake, adapter) = rig();
        let (ctx, _rx, reg) = ctx(adapter);
        let err = on_connected(fake.as_ref(), &ctx).await.unwrap_err();
        assert_eq!(err, "IBKR returned no account");
        assert!(reg.snapshot().iter().all(|h| h.connected_since.is_none()));
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
            tokio::spawn(async move { session(code, &slot, client, &ctx, &mut Seen::default(), &HEARTBEAT).await })
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

    /// Any docker call is a bug on an external gateway's path.
    struct NoDocker;

    impl GatewayDocker for NoDocker {
        fn inspect(&self, _: &str) -> Option<ContainerInfo> {
            panic!("docker inspect on an external gateway")
        }
        fn run(&self, _: &RunSpec) -> Result<(), String> {
            panic!("docker run on an external gateway")
        }
        fn stop_rm(&self, _: &str) -> Result<(), String> {
            panic!("docker rm on an external gateway")
        }
        fn logs_tail(&self, _: &str, _: usize) -> String {
            panic!("docker logs on an external gateway")
        }
        fn available(&self) -> bool {
            panic!("docker info on an external gateway")
        }
    }

    fn gw(docker: Arc<dyn GatewayDocker>, username: &str, gateway: Option<&str>, state_dir: PathBuf) -> Gateway {
        Gateway {
            docker,
            code: "IBKR_TWS-PAPER".into(),
            env: "PAPER",
            creds: IbkrTwsCredentials {
                username: username.into(),
                password: if username.is_empty() { String::new() } else { "pw".into() },
                gateway: gateway.map(str::to_string),
                master_client_id: 0,
            },
            state_dir,
        }
    }

    #[tokio::test]
    async fn external_gateway_skips_docker() {
        let gw = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let addr = attempt(&gw, |addr: String| async move { Ok::<_, String>(addr) }).await;
        assert_eq!(addr, Ok("ib-gateway:4004".to_string()));
    }

    /// Unreachable is a failed attempt (down, retried; the gateway restarts
    /// daily), never stopped, and must never read the logs, whose "login
    /// failed" would stop the stream.
    #[tokio::test]
    async fn unreachable_external_gateway_retries_not_stops() {
        let gw = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let next = attempt(&gw, |_: String| async { Err::<(), _>("connection refused") }).await;
        assert_eq!(next, Err(Next::Retry("IB Gateway at ib-gateway:4004 not reachable: connection refused".into())));
    }

    /// A running container of ours is adopted and reached on loopback.
    struct Adopt(ContainerInfo);

    impl GatewayDocker for Adopt {
        fn inspect(&self, _: &str) -> Option<ContainerInfo> {
            Some(self.0.clone())
        }
        fn run(&self, _: &RunSpec) -> Result<(), String> {
            panic!("adopted, not started")
        }
        fn stop_rm(&self, _: &str) -> Result<(), String> {
            panic!("adopted, not removed")
        }
        fn logs_tail(&self, _: &str, _: usize) -> String {
            "Login failed".into()
        }
        fn available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn managed_gateway_connects_on_loopback() {
        let dir = std::env::temp_dir().join(format!("oms-ibkr-attempt-{}", Uuid::new_v4()));
        let base = gw(Arc::new(NoDocker), "trader1", None, dir.clone());
        let info = ContainerInfo {
            running: true,
            api_port: Some(4123),
            container_port: Some(super::super::gateway::CONTAINER_PORT_PAPER),
            loopback_only: true,
            creds_tag: Some(super::super::gateway::creds_tag(&dir, &base.creds).unwrap()),
        };
        let gw = Gateway { docker: Arc::new(Adopt(info)), ..base };
        assert_eq!(attempt(&gw, |addr: String| async move { Ok::<_, String>(addr) }).await, Ok("127.0.0.1:4123".to_string()));
        // Managed: a refused connect reads the container's logs.
        let next = attempt(&gw, |_: String| async { Err::<(), _>("refused") }).await;
        assert!(matches!(next, Err(Next::Stop(m)) if m.contains("login failed")), "managed path classifies logs");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_hung_external_connect_times_out_and_retries() {
        let gw = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let next = attempt_within(&gw, Duration::from_millis(20), |_: String| std::future::pending::<Result<(), String>>()).await;
        assert_eq!(next, Err(Next::Retry("IB Gateway at ib-gateway:4004 not reachable: timed out".into())));
    }

    fn with_accounts(accounts: &[&str]) -> Arc<FakeTws> {
        let fake = Arc::new(FakeTws::new());
        *fake.accounts.lock().unwrap() = accounts.iter().map(|a| a.to_string()).collect();
        fake
    }

    /// Real money: a PAPER connection must never trade through a gateway
    /// logged into a live account, nor LIVE through a paper one.
    #[tokio::test]
    async fn an_external_gateway_must_match_the_environment() {
        let paper = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let live = Gateway { env: "LIVE", ..gw(Arc::new(NoDocker), "", Some("ib-gateway:4003"), PathBuf::from("/nonexistent")) };
        assert_eq!(
            external_mismatch(&paper, with_accounts(&["U123", "DU456"]).as_ref()).await,
            Some(Next::Stop("IB Gateway at ib-gateway:4004 is logged into a live account but this connection is PAPER".into()))
        );
        assert_eq!(
            external_mismatch(&live, with_accounts(&["DU456", "DF7"]).as_ref()).await,
            Some(Next::Stop("IB Gateway at ib-gateway:4003 is logged into a paper account but this connection is LIVE".into()))
        );
        assert_eq!(external_mismatch(&paper, with_accounts(&["DU456", "DF7"]).as_ref()).await, None);
        assert_eq!(external_mismatch(&live, with_accounts(&["U123"]).as_ref()).await, None);
        assert!(matches!(external_mismatch(&paper, with_accounts(&[]).as_ref()).await, Some(Next::Retry(_))), "no accounts: not trusted");
        // Managed: the OMS logged in itself with this environment's mode.
        let managed = gw(Arc::new(NoDocker), "trader1", None, PathBuf::from("/nonexistent"));
        assert_eq!(external_mismatch(&managed, with_accounts(&["U123"]).as_ref()).await, None);
    }

    // ---- supervise: what the health shows after each pass ----

    const FAST: Heartbeat = Heartbeat { every: Duration::from_millis(20), timeout: Duration::from_millis(20) };

    /// Polls the one stream's health until `ok` holds (or panics after ~2s).
    async fn until(reg: &StreamHealthRegistry, ok: impl Fn(&crate::stream_health::StreamHealth) -> bool) -> crate::stream_health::StreamHealth {
        for _ in 0..200 {
            let h = reg.snapshot().remove(0);
            if ok(&h) {
                return h;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let h = reg.snapshot().remove(0);
        panic!("never reached: {:?} {:?}", h.state, h.last_error);
    }

    fn slot() -> ClientSlot {
        Arc::new(std::sync::RwLock::new(None))
    }

    /// The reported bug: an unreachable external gateway read "connecting".
    #[tokio::test]
    async fn an_unreachable_external_gateway_is_down_with_a_retry_time() {
        let (_fake, adapter) = rig();
        let (ctx, _rx, reg) = ctx(adapter);
        let gw = gw(Arc::new(NoDocker), "", Some("openoms-ibgw:4004"), PathBuf::from("/nonexistent"));
        let task = tokio::spawn(supervise_with(gw, slot(), ctx, FAST, |_addr: String| async {
            Err::<Arc<dyn TwsClient>, _>("failed to lookup address information".to_string())
        }));
        let h = until(&reg, |h| h.state == StreamState::Down).await;
        assert_eq!(h.last_error.as_deref(), Some("IB Gateway at openoms-ibgw:4004 not reachable: failed to lookup address information"));
        assert!(h.next_retry_at.is_some_and(|t| t > chrono::Utc::now()), "down says when it retries");
        assert!(h.last_checked_at.is_some());
        task.abort();
    }

    #[tokio::test]
    async fn a_paper_live_mismatch_stops_the_stream() {
        let (_fake, adapter) = rig();
        let (ctx, _rx, reg) = ctx(adapter);
        let gw = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let live_account = with_accounts(&["U123"]);
        let task = tokio::spawn(supervise_with(gw, slot(), ctx, FAST, move |_addr: String| {
            let c = live_account.clone() as Arc<dyn TwsClient>;
            async move { Ok::<_, String>(c) }
        }));
        let h = until(&reg, |h| h.state == StreamState::Stopped).await;
        assert!(h.last_error.as_deref().is_some_and(|m| m.contains("live account but this connection is PAPER")));
        assert_eq!(h.next_retry_at, None);
        tokio::time::timeout(Duration::from_secs(1), task).await.expect("a stopped stream ends its task").unwrap();
    }

    #[tokio::test]
    async fn a_failed_login_stops_the_stream() {
        let (_fake, adapter) = rig();
        let (ctx, _rx, reg) = ctx(adapter);
        let dir = std::env::temp_dir().join(format!("oms-ibkr-stop-{}", Uuid::new_v4()));
        let base = gw(Arc::new(NoDocker), "trader1", None, dir.clone());
        let info = ContainerInfo {
            running: true,
            api_port: Some(4123),
            container_port: Some(super::super::gateway::CONTAINER_PORT_PAPER),
            loopback_only: true,
            creds_tag: Some(super::super::gateway::creds_tag(&dir, &base.creds).unwrap()),
        };
        let gw = Gateway { docker: Arc::new(Adopt(info)), ..base };
        let task = tokio::spawn(supervise_with(gw, slot(), ctx, FAST, |_addr: String| async {
            Err::<Arc<dyn TwsClient>, _>("refused".to_string())
        }));
        let h = until(&reg, |h| h.state == StreamState::Stopped).await;
        assert!(h.last_error.as_deref().is_some_and(|m| m.starts_with("IBKR login failed")));
        tokio::time::timeout(Duration::from_secs(1), task).await.expect("stopped").unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_live_session_pings_the_gateway_and_counts_it_as_a_check() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("DU1".into());
        let (ctx, _rx, reg) = ctx(adapter);
        let client: Arc<dyn TwsClient> = fake.clone();
        let task = tokio::spawn(async move { run_with(client, &ctx, &mut Seen::default(), &FAST).await });
        let live = until(&reg, |h| h.state == StreamState::Live).await;
        until(&reg, |h| h.last_checked_at > live.last_checked_at).await;
        assert!(fake.pings.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(reg.snapshot()[0].last_event_at, None, "a ping is not an order/fill event");
        task.abort();
    }

    #[tokio::test]
    async fn a_failed_ping_ends_the_session() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("DU1".into());
        *fake.ping_error.lock().unwrap() = Some("socket closed".into());
        let (ctx, _rx, _reg) = ctx(adapter);
        let err = run_with(fake.clone(), &ctx, &mut Seen::default(), &FAST).await.unwrap_err();
        assert_eq!(err, "IB Gateway did not answer the heartbeat: socket closed");
    }

    #[tokio::test]
    async fn a_hung_ping_times_out_and_ends_the_session() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("DU1".into());
        fake.ping_hangs.store(true, std::sync::atomic::Ordering::SeqCst);
        let (ctx, _rx, _reg) = ctx(adapter);
        let err = run_with(fake.clone(), &ctx, &mut Seen::default(), &FAST).await.unwrap_err();
        assert_eq!(err, "IB Gateway did not answer the heartbeat within 0s");
    }

    #[tokio::test]
    async fn a_heartbeat_failure_leaves_the_stream_down_and_retrying() {
        let (fake, adapter) = rig();
        fake.accounts.lock().unwrap().push("DU1".into());
        *fake.ping_error.lock().unwrap() = Some("socket closed".into());
        let (ctx, _rx, reg) = ctx(adapter);
        let gw = gw(Arc::new(NoDocker), "", Some("ib-gateway:4004"), PathBuf::from("/nonexistent"));
        let task = tokio::spawn(supervise_with(gw, slot(), ctx, FAST, move |_addr: String| {
            let c = fake.clone() as Arc<dyn TwsClient>;
            async move { Ok::<_, String>(c) }
        }));
        let h = until(&reg, |h| h.state == StreamState::Down).await;
        assert!(h.last_error.as_deref().is_some_and(|m| m.contains("heartbeat")), "{:?}", h.last_error);
        assert!(h.next_retry_at.is_some());
        task.abort();
    }
}
