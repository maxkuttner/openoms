//! Live updates for the trade cockpit: one WebSocket per open screen, fed from
//! both bus lanes.
//!
//! It sends notices, not data — "orders changed", "positions changed", "these
//! marks moved". The cockpit answers a notice by refetching through the REST
//! endpoints it already uses, so permission filtering stays in one place and a
//! socket reveals nothing a REST call wouldn't. Events are conflated per socket:
//! a burst of fills becomes one notice.

use std::collections::BTreeSet;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::app_state::AppState;
use crate::event_bus::{MarkUpdate, OrderLaneEvent};

/// How often pending order/position notices go out.
const FLUSH_MS: u64 = 250;
/// Marks tick constantly; at most one marks notice a second per socket.
const MARKS_FLUSH_MS: u64 = 1000;
/// An idle socket still gets a frame this often, so a half-open connection is
/// noticed: the client's watchdog reconnects when frames stop.
const HEARTBEAT_SECS: u64 = 15;
/// How often an open browser socket re-checks its session and marks it active.
/// Well inside the 30-minute idle limit.
const KEEPALIVE_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiNotice {
    Orders,
    Positions,
    Marks { instrument_ids: Vec<i64> },
    /// The socket fell behind the order lane: refetch everything.
    Resync,
    /// Nothing changed; proves the connection is alive.
    Heartbeat,
}

/// What has changed since the last flush.
#[derive(Default)]
pub struct Conflator {
    orders: bool,
    positions: bool,
    resync: bool,
    marks: BTreeSet<i64>,
}

impl Conflator {
    pub fn on_order(&mut self, r: Result<OrderLaneEvent, broadcast::error::RecvError>) {
        match r {
            Ok(OrderLaneEvent::Appended { .. }) => self.orders = true,
            Ok(OrderLaneEvent::PositionChanged { .. }) => self.positions = true,
            Err(broadcast::error::RecvError::Lagged(_)) => self.resync = true,
            Err(broadcast::error::RecvError::Closed) => {}
        }
    }

    pub fn on_mark(&mut self, m: MarkUpdate) {
        self.marks.insert(m.instrument_id);
    }

    /// Pending order/position/resync notices; a resync covers the other two.
    pub fn take_fast(&mut self) -> Vec<UiNotice> {
        let notices = if self.resync {
            vec![UiNotice::Resync]
        } else {
            [(self.orders, UiNotice::Orders), (self.positions, UiNotice::Positions)]
                .into_iter()
                .filter_map(|(pending, n)| pending.then_some(n))
                .collect()
        };
        (self.orders, self.positions, self.resync) = (false, false, false);
        notices
    }

    /// Pending marks as one notice (ids ascending), or nothing.
    pub fn take_marks(&mut self) -> Option<UiNotice> {
        if self.marks.is_empty() {
            return None;
        }
        let instrument_ids = std::mem::take(&mut self.marks).into_iter().collect();
        Some(UiNotice::Marks { instrument_ids })
    }
}

/// Cross-site WebSocket hijacking guard. A WebSocket upgrade is a GET, which the
/// auth middleware's CSRF check exempts, and browsers send cookies on it from
/// any page — so a browser upgrade (one with an `Origin`) must come from the
/// configured origin, exactly like a session write. Non-browser API-key clients
/// send no `Origin` and pass.
pub fn upgrade_allowed(headers: &HeaderMap, public_base_url: Option<&str>) -> bool {
    if !headers.contains_key("origin") {
        return true;
    }
    match public_base_url {
        Some(url) => crate::sessions::origin_is_allowed(headers, &Method::POST, url),
        None => false,
    }
}

/// Forward conflated notices to `out` until it closes. Only ever subscribes:
/// a slow socket lags its own receivers, never the bus.
pub async fn pump(
    mut order_rx: broadcast::Receiver<OrderLaneEvent>,
    mut price_rx: broadcast::Receiver<MarkUpdate>,
    out: mpsc::Sender<UiNotice>,
) {
    let mut conflator = Conflator::default();
    let mut fast = interval(Duration::from_millis(FLUSH_MS));
    let mut slow = interval(Duration::from_millis(MARKS_FLUSH_MS));
    let heartbeat_every = Duration::from_secs(HEARTBEAT_SECS);
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat_every, heartbeat_every);
    fast.set_missed_tick_behavior(MissedTickBehavior::Delay);
    slow.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut order_open, mut price_open) = (true, true);

    loop {
        let notices: Vec<UiNotice> = tokio::select! {
            r = order_rx.recv(), if order_open => {
                if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                    order_open = false;
                }
                conflator.on_order(r);
                continue;
            }
            r = price_rx.recv(), if price_open => {
                match r {
                    Ok(m) => conflator.on_mark(m),
                    // marks conflate anyway: skipping ahead loses nothing
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => price_open = false,
                }
                continue;
            }
            _ = fast.tick() => conflator.take_fast(),
            _ = slow.tick() => conflator.take_marks().into_iter().collect(),
            _ = heartbeat.tick() => vec![UiNotice::Heartbeat],
            _ = out.closed() => return,
        };
        for n in notices {
            if out.send(n).await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum KeepAlive {
    Touch(uuid::Uuid),
    Close,
}

/// What an open socket does with the session that opened it. The socket counts
/// as activity, so idle expiry does not apply while it is open — but a revoked
/// session (`None`) or one past its absolute cap ends the socket, so it never
/// outlives its login.
pub fn keepalive_step(session: Option<&crate::sessions::SessionRecord>, now: chrono::DateTime<chrono::Utc>) -> KeepAlive {
    match session {
        Some(s) if now < s.absolute_expires_at => KeepAlive::Touch(s.id),
        _ => KeepAlive::Close,
    }
}

/// `GET /updates`: upgrade to a WebSocket that streams [`UiNotice`]s as JSON
/// text frames until the client goes away.
pub async fn ws_updates(ws: WebSocketUpgrade, headers: HeaderMap, State(state): State<AppState>) -> Response {
    if !upgrade_allowed(&headers, state.session_config.public_base_url.as_deref()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // subscribe before the handshake completes, so nothing published in between is missed
    let bus = state.event_bus();
    let (order_rx, price_rx) = (bus.subscribe_order(), bus.subscribe_price());
    // the browser session that opened the socket, if any (API-key clients have none)
    let session_hash = crate::sessions::cookie_from_headers(
        &headers,
        crate::sessions::cookie_name(&state.session_config.cookie_policy),
    )
    .map(|token| crate::sessions::hash_session_token(&token));
    let pool = state.pool().clone();
    ws.on_upgrade(move |socket| serve_socket(socket, order_rx, price_rx, pool, session_hash))
}

async fn serve_socket(
    socket: WebSocket,
    order_rx: broadcast::Receiver<OrderLaneEvent>,
    price_rx: broadcast::Receiver<MarkUpdate>,
    pool: sqlx::PgPool,
    session_hash: Option<String>,
) {
    let (tx, mut notices) = mpsc::channel(64);
    let pumping = tokio::spawn(pump(order_rx, price_rx, tx));
    let (mut sink, mut incoming) = socket.split();
    let keepalive_every = Duration::from_secs(KEEPALIVE_SECS);
    let mut keepalive = tokio::time::interval_at(tokio::time::Instant::now() + keepalive_every, keepalive_every);
    loop {
        tokio::select! {
            _ = keepalive.tick(), if session_hash.is_some() => {
                let hash = session_hash.as_deref().unwrap_or_default();
                // a DB hiccup is not a reason to drop the trader: try again next tick
                let Ok(session) = crate::sessions::lookup_session(&pool, hash).await else { continue };
                match keepalive_step(session.as_ref(), chrono::Utc::now()) {
                    KeepAlive::Touch(id) => {
                        let _ = crate::sessions::touch_session(&pool, id).await;
                    }
                    KeepAlive::Close => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            n = notices.recv() => {
                let Some(n) = n else { break };
                let text = serde_json::to_string(&n).expect("a notice always serializes");
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            m = incoming.next() => match m {
                // the client never sends anything meaningful; pings are answered by axum
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    pumping.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;
    use uuid::Uuid;

    fn appended() -> OrderLaneEvent {
        OrderLaneEvent::Appended { order_id: Uuid::nil(), global_position: 1, event_type: "order_filled".into() }
    }

    fn position_changed() -> OrderLaneEvent {
        OrderLaneEvent::PositionChanged { portfolio_id: Uuid::nil(), instrument_id: "1".into() }
    }

    fn mark(instrument_id: i64) -> MarkUpdate {
        MarkUpdate { instrument_id, bid: 1.0, ask: 2.0 }
    }

    #[test]
    fn burst_conflates_to_one_notice_each() {
        let mut c = Conflator::default();
        for _ in 0..200 {
            c.on_order(Ok(appended()));
            c.on_order(Ok(position_changed()));
        }
        assert_eq!(c.take_fast(), vec![UiNotice::Orders, UiNotice::Positions]);
        assert_eq!(c.take_fast(), vec![]);
    }

    #[test]
    fn order_lag_becomes_resync() {
        let mut c = Conflator::default();
        c.on_order(Ok(appended()));
        c.on_order(Err(RecvError::Lagged(5)));
        assert_eq!(c.take_fast(), vec![UiNotice::Resync]);
    }

    #[test]
    fn marks_collect_distinct_ids() {
        let mut c = Conflator::default();
        for id in [3, 1, 3] {
            c.on_mark(mark(id));
        }
        assert_eq!(c.take_marks(), Some(UiNotice::Marks { instrument_ids: vec![1, 3] }));
        assert_eq!(c.take_marks(), None);
    }

    #[test]
    fn upgrade_refused_for_foreign_origin() {
        let with_origin = |o: &str| {
            let mut h = HeaderMap::new();
            h.insert("origin", o.parse().unwrap());
            h
        };
        let url = Some("https://oms.example");
        assert!(!upgrade_allowed(&with_origin("https://evil.example"), url));
        assert!(upgrade_allowed(&with_origin("https://oms.example"), url));
        assert!(upgrade_allowed(&HeaderMap::new(), url));
        assert!(!upgrade_allowed(&with_origin("https://oms.example"), None));
    }

    #[tokio::test]
    async fn socket_receives_orders_notice_after_fill() {
        use futures_util::StreamExt;

        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/oms_test_never_connects").unwrap();
        let state = crate::handlers::tests::test_app_state(pool);
        let bus = state.event_bus().clone();
        let app = axum::Router::new().route("/updates", axum::routing::get(ws_updates)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/updates")).await.expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish_order(appended());

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("a notice within 2s")
            .expect("open socket")
            .expect("frame");
        assert_eq!(frame.into_text().unwrap(), r#"{"type":"orders"}"#);
    }

    #[tokio::test]
    async fn pump_stops_when_receiver_closes() {
        let bus = crate::event_bus::EventBus::new();
        let (out, rx) = tokio::sync::mpsc::channel(64);
        drop(rx);
        let task = tokio::spawn(pump(bus.subscribe_order(), bus.subscribe_price(), out));
        bus.publish_order(appended());
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("pump returns once nobody reads")
            .unwrap();
    }

    /// A half-open connection (laptop sleep, NAT change) is only noticed if
    /// something is sent; an idle socket still gets a frame every HEARTBEAT_SECS.
    #[tokio::test(start_paused = true)]
    async fn pump_sends_heartbeat_when_idle() {
        let bus = crate::event_bus::EventBus::new();
        let (out, mut rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(pump(bus.subscribe_order(), bus.subscribe_price(), out));

        tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_SECS - 1)).await;
        assert!(rx.try_recv().is_err(), "no heartbeat before the interval");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert_eq!(rx.try_recv(), Ok(UiNotice::Heartbeat));
    }

    fn session(last_seen_mins_ago: i64, expires_in_mins: i64) -> crate::sessions::SessionRecord {
        let now = chrono::Utc::now();
        crate::sessions::SessionRecord {
            id: Uuid::from_u128(7),
            principal_id: Uuid::nil(),
            principal_code: "trader".into(),
            last_seen_at: now - chrono::Duration::minutes(last_seen_mins_ago),
            absolute_expires_at: now + chrono::Duration::minutes(expires_in_mins),
        }
    }

    /// An open socket is activity: a trader watching a quiet market must not be
    /// idled out, so the session is touched even past the idle limit.
    #[test]
    fn open_socket_keeps_an_idle_session_alive() {
        let r = session(45, 60);
        assert_eq!(keepalive_step(Some(&r), chrono::Utc::now()), KeepAlive::Touch(Uuid::from_u128(7)));
    }

    #[test]
    fn socket_closes_at_the_absolute_session_cap() {
        let r = session(1, -1);
        assert_eq!(keepalive_step(Some(&r), chrono::Utc::now()), KeepAlive::Close);
    }

    #[test]
    fn socket_closes_when_the_session_is_revoked() {
        assert_eq!(keepalive_step(None, chrono::Utc::now()), KeepAlive::Close);
    }

    #[test]
    fn notices_serialize_with_type_tag() {
        assert_eq!(serde_json::to_string(&UiNotice::Orders).unwrap(), r#"{"type":"orders"}"#);
        assert_eq!(
            serde_json::to_string(&UiNotice::Marks { instrument_ids: vec![1, 3] }).unwrap(),
            r#"{"type":"marks","instrument_ids":[1,3]}"#
        );
    }
}
