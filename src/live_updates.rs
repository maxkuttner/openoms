//! Live updates for the trade cockpit: one WebSocket per open screen, fed from
//! both bus lanes.
//!
//! It sends notices, not data — "orders changed", "positions changed", "these
//! marks moved". The cockpit answers a notice by refetching through the REST
//! endpoints it already uses, so permission filtering stays in one place and a
//! socket reveals nothing a REST call wouldn't. Events are conflated per socket:
//! a burst of fills becomes one notice. A socket is also only woken by orders
//! and positions its principal could fetch (see [`StreamScope`]).

use std::collections::{BTreeSet, HashSet};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::authz::{visible_portfolios, Permission};
use crate::event_bus::{AuthChanged, MarkUpdate, OrderLaneEvent};
use uuid::Uuid;

/// How often pending order/position notices go out.
const FLUSH_MS: u64 = 250;
/// Marks tick constantly; at most one marks notice a second per socket.
const MARKS_FLUSH_MS: u64 = 1000;
/// An idle socket still gets a frame this often, so a half-open connection is
/// noticed: the client's watchdog reconnects when frames stop. Each tick also
/// re-checks the socket's credential and reloads its scope (spec §8: a revoked
/// login loses its stream within this bound even if no change was announced).
const HEARTBEAT_SECS: u64 = 15;
/// The close-frame reason a socket ends with when its credential stops being valid.
pub const AUTH_REVOKED_REASON: &str = r#"{"type":"auth_revoked"}"#;

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
    /// The socket's login or key is no longer valid. Never sent as a text frame:
    /// the socket closes with [`AUTH_REVOKED_REASON`].
    AuthRevoked,
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

/// The portfolios one socket's principal may see, mirroring the REST routes:
/// orders as `list_orders` scopes them, positions as `get_portfolio_positions`.
/// Reloaded on every heartbeat and at once on an [`AuthChanged`] for the
/// principal, so a changed grant or role applies immediately (and within
/// [`HEARTBEAT_SECS`] even if the announcement was missed).
#[derive(Default)]
pub struct StreamScope {
    view_all: HashSet<Uuid>,
    view_own: HashSet<Uuid>,
    positions: HashSet<Uuid>,
}

impl StreamScope {
    pub async fn load(pool: &sqlx::PgPool, principal_id: Uuid) -> Result<Self, sqlx::Error> {
        let set = |v: Vec<Uuid>| v.into_iter().collect::<HashSet<_>>();
        Ok(Self {
            view_all: set(visible_portfolios(pool, principal_id, Permission::OrdersViewAll).await?),
            view_own: set(visible_portfolios(pool, principal_id, Permission::OrdersViewOwn).await?),
            positions: set(visible_portfolios(pool, principal_id, Permission::PositionsView).await?),
        })
    }

    pub fn sees_order(&self, principal_id: Uuid, portfolio_id: Uuid, owner: Uuid) -> bool {
        self.view_all.contains(&portfolio_id) || (owner == principal_id && self.view_own.contains(&portfolio_id))
    }

    pub fn sees_positions(&self, portfolio_id: Uuid) -> bool {
        self.positions.contains(&portfolio_id)
    }
}

/// What authenticated the socket, re-checked on every tick and on every
/// [`AuthChanged`] for its principal.
#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    /// A browser session, by token hash: live, not revoked, under its absolute
    /// cap, principal ACTIVE (all enforced by `lookup_session` + `keepalive_step`).
    Session(String),
    /// An API key, by `key_id`: not revoked and principal ACTIVE.
    ApiKey(String),
    /// Anything else (host token, strategy session): principal ACTIVE.
    Principal,
}

/// Result of re-checking a [`Credential`].
#[derive(Debug, PartialEq)]
pub enum CredentialCheck {
    /// Still valid; a session to mark active, if one is due a touch.
    Valid(Option<Uuid>),
    Revoked,
}

impl Credential {
    async fn check(&self, pool: &sqlx::PgPool, principal_id: Uuid) -> Result<CredentialCheck, sqlx::Error> {
        let valid = |ok: bool| if ok { CredentialCheck::Valid(None) } else { CredentialCheck::Revoked };
        match self {
            Credential::Session(hash) => {
                let session = crate::sessions::lookup_session(pool, hash).await?;
                let now = chrono::Utc::now();
                Ok(match keepalive_step(session.as_ref(), now) {
                    KeepAlive::Close => CredentialCheck::Revoked,
                    KeepAlive::Touch(id) => {
                        let due = session.is_some_and(|s| crate::sessions::needs_touch(now, s.last_seen_at));
                        CredentialCheck::Valid(due.then_some(id))
                    }
                })
            }
            Credential::ApiKey(key_id) => {
                let ok: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM api_key k JOIN principal p ON p.id = k.principal_id \
                      WHERE k.key_id = $1 AND k.principal_id = $2 AND k.revoked_at IS NULL AND p.status = 'ACTIVE')",
                )
                .bind(key_id)
                .bind(principal_id)
                .fetch_one(pool)
                .await?;
                Ok(valid(ok))
            }
            Credential::Principal => {
                let ok: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principal WHERE id = $1 AND status = 'ACTIVE')")
                        .bind(principal_id)
                        .fetch_one(pool)
                        .await?;
                Ok(valid(ok))
            }
        }
    }
}

/// Who a socket belongs to, how they authenticated, and what they may see.
pub struct Viewer {
    pub pool: sqlx::PgPool,
    pub principal_id: Uuid,
    pub credential: Credential,
    pub scope: StreamScope,
}

impl Viewer {
    /// Whether this event concerns something the viewer may fetch. An order is
    /// looked up for its portfolio and owner; a failed lookup hides it.
    async fn sees(&self, ev: &OrderLaneEvent) -> bool {
        match ev {
            OrderLaneEvent::PositionChanged { portfolio_id, .. } => self.scope.sees_positions(*portfolio_id),
            OrderLaneEvent::Appended { order_id, .. } => {
                if self.scope.view_all.is_empty() && self.scope.view_own.is_empty() {
                    return false;
                }
                let row: Result<Option<(Uuid, Uuid)>, _> =
                    sqlx::query_as("SELECT portfolio_id, principal_id FROM order_state WHERE order_id = $1")
                        .bind(order_id)
                        .fetch_optional(&self.pool)
                        .await;
                matches!(row, Ok(Some((portfolio, owner))) if self.scope.sees_order(self.principal_id, portfolio, owner))
            }
        }
    }

    /// Re-check the credential, then reload the scope. `false` once the
    /// credential is revoked. A DB hiccup keeps the socket and its last known
    /// scope rather than dropping the trader: the next tick tries again.
    async fn recheck(&mut self) -> bool {
        match self.credential.check(&self.pool, self.principal_id).await {
            Ok(CredentialCheck::Revoked) => return false,
            Ok(CredentialCheck::Valid(Some(session_id))) => {
                let _ = crate::sessions::touch_session(&self.pool, session_id).await;
            }
            Ok(CredentialCheck::Valid(None)) | Err(_) => {}
        }
        if let Ok(scope) = StreamScope::load(&self.pool, self.principal_id).await {
            self.scope = scope;
        }
        true
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

/// The bus receivers one socket listens on.
pub struct Lanes {
    pub order: broadcast::Receiver<OrderLaneEvent>,
    pub price: broadcast::Receiver<MarkUpdate>,
    pub auth: broadcast::Receiver<AuthChanged>,
}

impl Lanes {
    pub fn subscribe(bus: &crate::event_bus::EventBus) -> Self {
        Self { order: bus.subscribe_order(), price: bus.subscribe_price(), auth: bus.subscribe_auth() }
    }
}

/// Forward conflated notices to `out` until it closes, dropping order-lane
/// events outside `viewer`'s scope. Re-checks the credential on every heartbeat
/// and on an [`AuthChanged`] for the viewer; a revoked credential sends
/// [`UiNotice::AuthRevoked`] and ends the pump. Only ever subscribes: a slow
/// socket lags its own receivers, never the bus.
pub async fn pump(lanes: Lanes, out: mpsc::Sender<UiNotice>, mut viewer: Viewer) {
    let Lanes { order: mut order_rx, price: mut price_rx, auth: mut auth_rx } = lanes;
    let mut conflator = Conflator::default();
    let mut fast = interval(Duration::from_millis(FLUSH_MS));
    let mut slow = interval(Duration::from_millis(MARKS_FLUSH_MS));
    let heartbeat_every = Duration::from_secs(HEARTBEAT_SECS);
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat_every, heartbeat_every);
    fast.set_missed_tick_behavior(MissedTickBehavior::Delay);
    slow.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut order_open, mut price_open, mut auth_open) = (true, true, true);
    let mut recheck_due = false;

    loop {
        let notices: Vec<UiNotice> = tokio::select! {
            r = order_rx.recv(), if order_open => {
                if matches!(r, Err(broadcast::error::RecvError::Closed)) {
                    order_open = false;
                }
                if let Ok(ev) = &r {
                    if !viewer.sees(ev).await {
                        continue;
                    }
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
            r = auth_rx.recv(), if auth_open => {
                match r {
                    Ok(AuthChanged { principal_id }) if principal_id != viewer.principal_id => continue,
                    // a missed announcement might have been ours: re-check to be safe
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        auth_open = false;
                        continue;
                    }
                }
                if !viewer.recheck().await {
                    let _ = out.send(UiNotice::AuthRevoked).await;
                    return;
                }
                continue;
            }
            _ = fast.tick() => conflator.take_fast(),
            _ = slow.tick() => conflator.take_marks().into_iter().collect(),
            _ = heartbeat.tick() => {
                recheck_due = true;
                vec![UiNotice::Heartbeat]
            }
            _ = out.closed() => return,
        };
        for n in notices {
            if out.send(n).await.is_err() {
                return;
            }
        }
        // after the heartbeat is out, so a slow database never delays it
        if std::mem::take(&mut recheck_due) && !viewer.recheck().await {
            let _ = out.send(UiNotice::AuthRevoked).await;
            return;
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
/// session (`None`, which includes a disabled principal) or one past its
/// absolute cap ends the socket, so it never outlives its login.
pub fn keepalive_step(session: Option<&crate::sessions::SessionRecord>, now: chrono::DateTime<chrono::Utc>) -> KeepAlive {
    match session {
        Some(s) if now < s.absolute_expires_at => KeepAlive::Touch(s.id),
        _ => KeepAlive::Close,
    }
}

/// `GET /updates`: upgrade to a WebSocket that streams [`UiNotice`]s as JSON
/// text frames until the client goes away.
pub async fn ws_updates(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Response {
    if !upgrade_allowed(&headers, state.session_config.public_base_url.as_deref()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let scope = match StreamScope::load(state.pool(), auth.principal_id).await {
        Ok(scope) => scope,
        Err(err) => return crate::handlers::internal("failed to load permissions", err).into_response(),
    };
    let credential = socket_credential(&auth, &headers, &state.session_config.cookie_policy);
    let viewer = Viewer { pool: state.pool().clone(), principal_id: auth.principal_id, credential, scope };
    // subscribe before the handshake completes, so nothing published in between is missed
    let lanes = Lanes::subscribe(state.event_bus());
    ws.on_upgrade(move |socket| serve_socket(socket, lanes, viewer))
}

/// The credential to re-check for this socket. A browser session is keyed by
/// its cookie; an API key by its `key_id` (the auth middleware already
/// verified the secret); anything else by the principal alone.
fn socket_credential(auth: &AuthContext, headers: &HeaderMap, policy: &crate::sessions::CookiePolicy) -> Credential {
    use crate::auth::AuthMethod;
    match auth.auth_method {
        AuthMethod::Oidc | AuthMethod::Local | AuthMethod::LegacyAdmin => {
            match crate::sessions::cookie_from_headers(headers, crate::sessions::cookie_name(policy)) {
                Some(token) => Credential::Session(crate::sessions::hash_session_token(&token)),
                None => Credential::Principal,
            }
        }
        AuthMethod::ApiKey => match crate::auth::trading_key_id(headers) {
            Some(key_id) => Credential::ApiKey(key_id),
            None => Credential::Principal,
        },
        AuthMethod::HostCli => Credential::Principal,
    }
}

async fn serve_socket(socket: WebSocket, lanes: Lanes, viewer: Viewer) {
    let (tx, mut notices) = mpsc::channel(64);
    let pumping = tokio::spawn(pump(lanes, tx, viewer));
    let (mut sink, mut incoming) = socket.split();
    loop {
        tokio::select! {
            n = notices.recv() => {
                let Some(n) = n else { break };
                if n == UiNotice::AuthRevoked {
                    let frame = axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::POLICY,
                        reason: AUTH_REVOKED_REASON.into(),
                    };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    break;
                }
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

    /// A socket whose principal sees nothing; its pool never connects.
    fn nobody() -> Viewer {
        Viewer {
            pool: sqlx::PgPool::connect_lazy("postgres://localhost/oms_test_never_connects").unwrap(),
            principal_id: Uuid::nil(),
            credential: Credential::Principal,
            scope: StreamScope::default(),
        }
    }

    #[test]
    fn scope_mirrors_view_all_and_view_own() {
        let (me, other, all, own) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(10), Uuid::from_u128(11));
        let scope = StreamScope {
            view_all: [all].into(),
            view_own: [own].into(),
            positions: [own].into(),
        };
        assert!(scope.sees_order(me, all, other));
        assert!(scope.sees_order(me, own, me));
        assert!(!scope.sees_order(me, own, other), "another's order in a view_own portfolio");
        assert!(!scope.sees_order(me, Uuid::from_u128(12), me), "a portfolio outside scope");
        assert!(scope.sees_positions(own) && !scope.sees_positions(all));
    }

    /// Against a migrated database (see `handlers::tests::test_pool`): an order in
    /// a portfolio the principal cannot see wakes nobody; one it can see does.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn ws_updates_skip_orders_outside_scope() {
        use crate::handlers::tests::{seed_instrument, seed_order_state, test_app_state, test_pool};
        use futures_util::StreamExt;

        let pool = test_pool().await;
        let me = Uuid::new_v4();
        let code = format!("ws-scope-{me}");
        sqlx::query("INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')")
            .bind(me).bind(&code).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'trader')")
            .bind(me).execute(&pool).await.unwrap();
        let instrument = seed_instrument(&pool, "WS").await;
        let mine = seed_order_state(&pool, me, instrument, "ws-mine").await;
        let foreign = seed_order_state(&pool, me, instrument, "ws-foreign").await;
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) \
             SELECT $1, $2, portfolio_id, true, true FROM order_state WHERE order_id = $3",
        )
        .bind(Uuid::new_v4()).bind(me).bind(mine).execute(&pool).await.unwrap();

        let state = test_app_state(pool);
        let bus = state.event_bus().clone();
        let auth = AuthContext::machine(me, code);
        let app = axum::Router::new()
            .route("/updates", axum::routing::get(ws_updates))
            .layer(axum::Extension(auth))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/updates")).await.expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let appended = |order_id| OrderLaneEvent::Appended { order_id, global_position: 1, event_type: "order_filled".into() };

        bus.publish_order(appended(foreign));
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(800), socket.next()).await;
        assert!(quiet.is_err(), "an order outside scope must not produce a notice");

        bus.publish_order(appended(mine));
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("a notice within 2s")
            .expect("open socket")
            .expect("frame");
        assert_eq!(frame.into_text().unwrap(), r#"{"type":"orders"}"#);
    }

    /// A signed-in local user with a role and a live session, on a server whose
    /// `/updates` sees that session's cookie. Returns (principal, cookie value,
    /// state, socket address).
    async fn signed_in_socket_server() -> (Uuid, String, AppState, std::net::SocketAddr) {
        use crate::handlers::tests::{test_app_state, test_pool};
        let pool = test_pool().await;
        let me = Uuid::new_v4();
        let code = format!("ws-auth-{me}");
        sqlx::query("INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')")
            .bind(me).bind(&code).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'trader')")
            .bind(me).execute(&pool).await.unwrap();
        let mut state = test_app_state(pool.clone());
        state.session_config.public_base_url = Some(WS_TEST_BASE.to_string());
        let token = crate::sessions::create_session(
            &pool, me, crate::sessions::AuthMethod::Local, true, &crate::sessions::SessionTtl::default(), None,
        )
        .await
        .unwrap();
        let auth = AuthContext {
            principal_id: me,
            principal_code: code,
            auth_method: crate::auth::AuthMethod::Local,
            mfa: true,
        };
        let app = axum::Router::new()
            .route("/updates", axum::routing::get(ws_updates))
            .layer(axum::Extension(auth))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (me, token.plaintext, state, addr)
    }

    const WS_TEST_BASE: &str = "http://localhost:3001";

    fn cookie_header(state: &AppState, value: &str) -> String {
        format!("{}={value}", crate::sessions::cookie_name(&state.session_config.cookie_policy))
    }

    async fn connect_with_cookie(
        addr: std::net::SocketAddr,
        cookie: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}/updates").into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(req).await.expect("connect");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        socket
    }

    /// The next close frame within `within`, skipping notices; panics if none.
    async fn close_reason<S>(socket: &mut S, within: std::time::Duration) -> String
    where
        S: futures_util::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message as T;
        tokio::time::timeout(within, async {
            loop {
                match socket.next().await {
                    Some(Ok(T::Close(frame))) => return frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                    Some(Ok(_)) => continue,
                    other => panic!("socket ended without a close frame: {other:?}"),
                }
            }
        })
        .await
        .expect("a close frame well before the 15 s tick")
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn ws_closes_when_principal_disabled() {
        let (me, cookie, state, addr) = signed_in_socket_server().await;
        let mut socket = connect_with_cookie(addr, &cookie_header(&state, &cookie)).await;
        let admin = AuthContext::machine(Uuid::new_v4(), "ws-test-admin".into());
        crate::users_api::disable_user(axum::extract::State(state.clone()), axum::Extension(admin), axum::extract::Path(me))
            .await
            .unwrap();
        assert_eq!(close_reason(&mut socket, std::time::Duration::from_secs(3)).await, r#"{"type":"auth_revoked"}"#);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn ws_closes_on_logout_all() {
        let (_me, cookie, state, addr) = signed_in_socket_server().await;
        let header = cookie_header(&state, &cookie);
        let mut socket = connect_with_cookie(addr, &header).await;
        let mut h = HeaderMap::new();
        h.insert("cookie", header.parse().unwrap());
        h.insert("origin", WS_TEST_BASE.parse().unwrap());
        let resp = crate::local_auth_api::logout_all(axum::extract::State(state.clone()), h).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(close_reason(&mut socket, std::time::Duration::from_secs(3)).await, r#"{"type":"auth_revoked"}"#);
    }

    /// A grant added by an admin wakes the socket for that portfolio at once,
    /// not at the next 15 s scope reload. Real time against Postgres, bounded
    /// to 3 s (paused time would auto-advance onto the 15 s tick while the
    /// test waits on socket I/O and pass for the wrong reason).
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn ws_scope_refreshes_immediately_on_grant_change() {
        use crate::handlers::tests::{seed_instrument, seed_order_state};
        use futures_util::StreamExt;
        let (me, cookie, state, addr) = signed_in_socket_server().await;
        let pool = state.pool().clone();
        let instrument = seed_instrument(&pool, "WSG").await;
        let order = seed_order_state(&pool, me, instrument, "ws-grant").await;
        let portfolio: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM order_state WHERE order_id = $1")
            .bind(order).fetch_one(&pool).await.unwrap();
        let mut socket = connect_with_cookie(addr, &cookie_header(&state, &cookie)).await;
        let bus = state.event_bus().clone();
        let appended = || OrderLaneEvent::Appended { order_id: order, global_position: 1, event_type: "order_filled".into() };

        bus.publish_order(appended());
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(600), socket.next()).await;
        assert!(quiet.is_err(), "no grant yet: no notice");

        let admin = AuthContext::machine(Uuid::new_v4(), "ws-test-admin".into());
        crate::users_api::set_portfolios(
            axum::extract::State(state.clone()),
            axum::Extension(admin),
            axum::extract::Path(me),
            axum::Json(vec![crate::users_api::PortfolioAccess { portfolio_id: portfolio, can_trade: false }]),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        bus.publish_order(appended());
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
            .await
            .expect("a notice right after the grant")
            .expect("open socket")
            .expect("frame");
        assert_eq!(frame.into_text().unwrap(), r#"{"type":"orders"}"#);
    }

    #[tokio::test]
    async fn pump_stops_when_receiver_closes() {
        let bus = crate::event_bus::EventBus::new();
        let (out, rx) = tokio::sync::mpsc::channel(64);
        drop(rx);
        let task = tokio::spawn(pump(Lanes::subscribe(&bus), out, nobody()));
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
        tokio::spawn(pump(Lanes::subscribe(&bus), out, nobody()));

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
            auth_method: crate::sessions::AuthMethod::Oidc,
            mfa_verified_at: None,
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
