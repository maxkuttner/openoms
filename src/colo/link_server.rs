//! The control link's OMS side: a TLS WebSocket on its own port. A
//! co-located strategy logs in with its trading token — Kill does not cut
//! this — gets its full state first, then pushes; it acknowledges limits and
//! kills and heartbeats every second.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use colo_link::proto::{FromStrategy, ToStrategy, HEARTBEAT};
use colo_link::v2::{
    Authority, Bootstrap, EventAck as V2EventAck, FromRuntime, ToRuntime, ToRuntimeEnvelope,
    PROTOCOL_VERSION,
};
use uuid::Uuid;

use super::deployment::{self, Deployment, V2Binding};
use super::ColoHub;
use crate::app_state::AppState;
use crate::config::ControlLinkSettings;
use crate::domain::orders::risk::TradingState;
use crate::strategies::Mode;

/// A strategy not heard for 3 heartbeats is gone, even if its TCP connection
/// is not.
pub fn silent(last_heard: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(last_heard) > 3 * HEARTBEAT
}

/// Who may connect, and what they are told first.
#[async_trait::async_trait]
pub trait LinkBackend: Send + Sync {
    /// The co-located strategy a token and peer belong to; `None` refuses.
    async fn authorize(&self, token: &str, peer: SocketAddr) -> Option<String>;
    async fn initial_state(&self, code: &str) -> ToStrategy;
    async fn authorize_v2(
        &self,
        _token: &str,
        _peer: SocketAddr,
        _deployment_id: Uuid,
        _session_nonce: Uuid,
        _control: bool,
    ) -> Option<V2Binding> {
        None
    }
    async fn bootstrap_v2(&self, _deployment: &Deployment) -> Result<Bootstrap, String> {
        Err("v2 bootstrap not supported".into())
    }
    /// A runtime message that is not an event batch: acknowledgements, hello
    /// and recovery status. Ignored unless the backend records it.
    async fn runtime_message(&self, _deployment: &Deployment, _message: &FromRuntime) -> Result<(), String> {
        Ok(())
    }
    async fn accept_events(
        &self,
        _deployment: &Deployment,
        _batch: &colo_link::v2::EventBatch,
    ) -> Result<u64, String> {
        Err("v2 event ingest not supported".into())
    }
}

/// The link's TLS server identity. rustls, not the platform's TLS: on macOS
/// that would import the private key into a keychain and prompt for it.
pub fn tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>().map_err(|e| format!("certificate: {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("private key: {e}"))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Accept links on `listener` until the process ends.
pub async fn serve_on(listener: TcpListener, acceptor: tokio_rustls::TlsAcceptor, backend: Arc<dyn LinkBackend>, hub: ColoHub) {
    loop {
        let Ok((tcp, peer)) = listener.accept().await else { continue };
        let (acceptor, backend, hub) = (acceptor.clone(), backend.clone(), hub.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(tcp, peer, acceptor, backend, hub).await {
                warn!(%peer, error = %e, "control link: connection ended");
            }
        });
    }
}

async fn handle(
    tcp: tokio::net::TcpStream,
    peer: SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    backend: Arc<dyn LinkBackend>,
    hub: ColoHub,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tls = acceptor.accept(tcp).await?;
    let mut token = String::new();
    let mut path = String::new();
    let mut query = String::new();
    let ws = tokio_tungstenite::accept_hdr_async(tls, |req: &Request, res: Response| {
        if let Some(v) = req.headers().get("Authorization").and_then(|v| v.to_str().ok()) {
            token = v.strip_prefix("Bearer ").unwrap_or("").to_string();
        }
        path = req.uri().path().to_string();
        query = req.uri().query().unwrap_or("").to_string();
        Ok(res)
    })
    .await?;
    if path == "/v2/control" || path == "/v2/events" {
        return handle_v2(ws, &token, peer, &path, &query, backend, hub).await;
    }
    let Some(code) = backend.authorize(&token, peer).await else {
        warn!(%peer, "control link: refused (unknown token, not co-located, or address not allowed)");
        return Ok(());
    };
    let (mut sink, mut stream) = ws.split();
    let (conn, mut pushes) = hub.connect(&code);
    info!(strategy = %code, %peer, "control link: connected");
    sink.send(Message::Text(serde_json::to_string(&backend.initial_state(&code).await)?)).await?;
    let mut beat = tokio::time::interval(HEARTBEAT);
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        loop {
            tokio::select! {
                push = pushes.recv() => match push {
                    Some(msg) => sink.send(Message::Text(serde_json::to_string(&msg)?)).await?,
                    None => return Ok(()),
                },
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<FromStrategy>(&text) {
                        Ok(msg) => hub.heard(&code, conn, &msg),
                        Err(e) => warn!(strategy = %code, error = %e, "control link: unreadable message"),
                    },
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                },
                _ = beat.tick() => {
                    if hub.last_heard(&code, conn).is_some_and(|t| silent(t, std::time::Instant::now())) {
                        return Err("silent for 3 heartbeats".into());
                    }
                    sink.send(Message::Text(serde_json::to_string(&ToStrategy::Heartbeat)?)).await?
                }
            }
        }
    }
    .await;
    hub.disconnect(&code, conn);
    info!(strategy = %code, "control link: disconnected");
    result
}

/// The OMS's backend: trading tokens, the strategy registry and its positions.

type ServerWs = tokio_tungstenite::WebSocketStream<
    tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
>;

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

async fn handle_v2(
    ws: ServerWs,
    token: &str,
    peer: SocketAddr,
    path: &str,
    query: &str,
    backend: Arc<dyn LinkBackend>,
    hub: ColoHub,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let deployment_id = query_param(query, "deployment")
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or("v2: missing deployment")?;
    let session_nonce = query_param(query, "nonce")
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or("v2: missing session nonce")?;
    let control = path == "/v2/control";
    let Some(binding) = backend
        .authorize_v2(token, peer, deployment_id, session_nonce, control)
        .await
    else {
        warn!(%peer, "v2 link: refused (unknown token, deployment or session)");
        return Ok(());
    };
    let (mut sink, mut stream) = ws.split();
    let mut beat = tokio::time::interval(HEARTBEAT);
    let mut last_seen = Instant::now();

    let mut pushes = if control {
        let bootstrap = backend
            .bootstrap_v2(&binding.deployment)
            .await
            .map_err(|e| format!("v2 bootstrap: {e}"))?;
        let revision = bootstrap.baseline.sequence;
        let first = ToRuntimeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            account: binding.account.clone(),
            epoch: binding.epoch,
            policy_revision: revision,
            sequence: 1,
            session_nonce: binding.session_nonce.to_string(),
            message: ToRuntime::Bootstrap { bootstrap },
        };
        sink.send(Message::Text(serde_json::to_string(&first)?)).await?;
        let (conn, rx) = hub.register_v2_control(
            binding.deployment.id,
            binding.account.clone(),
            binding.epoch,
            revision,
            binding.session_nonce.to_string(),
        );
        hub.push_v2_control(
            binding.deployment.id,
            ToRuntime::Authority {
                authority: Authority { valid_for_secs: binding.authority_secs as u64 },
            },
        );
        Some((conn, rx))
    } else {
        None
    };
    let session = binding.session_nonce.to_string();
    let mut authority = tokio::time::interval(Duration::from_secs(
        u64::from(binding.authority_secs.max(2) / 2),
    ));
    authority.tick().await;
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        loop {
            tokio::select! {
                _ = authority.tick(), if control => {
                    hub.push_v2_control(
                        binding.deployment.id,
                        ToRuntime::Authority {
                            authority: Authority { valid_for_secs: binding.authority_secs as u64 },
                        },
                    );
                }
                push = async {
                    match pushes.as_mut() {
                        Some((_, rx)) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match push {
                        Some(msg) => sink.send(Message::Text(serde_json::to_string(&msg)?)).await?,
                        None => return Ok(()),
                    }
                }
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<colo_link::v2::FromRuntimeEnvelope>(&text) {
                        Ok(msg) => {
                            if msg.protocol_version != PROTOCOL_VERSION
                                || msg.account != binding.account
                                || msg.epoch != binding.epoch
                                || msg.session_nonce != session
                            {
                                warn!(deployment = %deployment_id, "v2 link: envelope does not match the bound deployment");
                            } else {
                                match &msg.message {
                                    FromRuntime::Events { batch } => {
                                        if let Err(e) = backend.accept_events(&binding.deployment, batch).await {
                                            warn!(deployment = %deployment_id, error = %e, "v2 link: durable event accept failed");
                                        }
                                    }
                                    FromRuntime::Heartbeat { .. } => last_seen = Instant::now(),
                                    other => {
                                        if let Err(e) = backend.runtime_message(&binding.deployment, other).await {
                                            warn!(deployment = %deployment_id, error = %e, "v2 link: runtime message not recorded");
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => warn!(error = %e, "v2 link: unreadable envelope"),
                    },
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                },
                _ = beat.tick() => {
                    if last_seen.elapsed() > Duration::from_secs(3) {
                        return Err("v2 link: silent for 3 heartbeats".into());
                    }
                    sink.send(Message::Ping(Vec::new())).await?;
                }
            }
        }
    }
    .await;
    if let Some((conn, _)) = pushes {
        hub.disconnect_v2_control(deployment_id, conn);
    }
    result
}

struct AppBackend(AppState);

#[async_trait::async_trait]
impl LinkBackend for AppBackend {
    async fn authorize(&self, token: &str, peer: SocketAddr) -> Option<String> {
        let (key_id, secret) = token.split_once('.')?;
        let (principal_id, _) = crate::auth::verify_key(self.0.pool(), key_id, secret).await.ok()??;
        let strategy = self.0.strategies().all().iter().find(|s| s.principal_id == principal_id && s.mode == Mode::Colocated).cloned()?;
        let ip = peer.ip().to_string();
        (strategy.allowed_addresses.is_empty() || strategy.allowed_addresses.contains(&ip)).then_some(strategy.code)
    }

    async fn authorize_v2(
        &self,
        token: &str,
        peer: SocketAddr,
        deployment_id: Uuid,
        session_nonce: Uuid,
        control: bool,
    ) -> Option<V2Binding> {
        let (key_id, secret) = token.split_once('.')?;
        let (principal_id, _) = crate::auth::verify_key(self.0.pool(), key_id, secret).await.ok()??;
        let strategy = self
            .0
            .strategies()
            .all()
            .iter()
            .find(|s| s.principal_id == principal_id && s.mode == Mode::Colocated)?
            .clone();
        let ip = peer.ip().to_string();
        if !strategy.allowed_addresses.is_empty() && !strategy.allowed_addresses.contains(&ip) {
            return None;
        }
        deployment::authorize_v2(self.0.pool(), &strategy.code, deployment_id, session_nonce, control)
            .await
            .ok()
    }

    async fn bootstrap_v2(&self, deployment: &Deployment) -> Result<Bootstrap, String> {
        deployment::bootstrap(&self.0, deployment).await.map_err(|e| e.to_string())
    }

    async fn accept_events(
        &self,
        deployment: &Deployment,
        batch: &colo_link::v2::EventBatch,
    ) -> Result<u64, String> {
        let ack = super::ingest::accept_batch(self.0.pool(), deployment, batch)
            .await
            .map_err(|e| e.to_string())?;
        self.0.colo().push_v2_control(
            deployment.id,
            ToRuntime::EventAck {
                ack: V2EventAck { through_sequence: ack.through_sequence },
            },
        );
        if let Err(e) = super::ingest::drain_inbox(&self.0, deployment.id).await {
            warn!(deployment = %deployment.id, error = %e, "v2 link: inbox drain deferred");
        }
        Ok(ack.through_sequence)
    }

    async fn runtime_message(&self, deployment: &Deployment, message: &FromRuntime) -> Result<(), String> {
        let pool = self.0.pool();
        match message {
            FromRuntime::ControlAck { epoch, command_id, stage } => {
                let moved = deployment::record_control_ack(pool, deployment.id, *epoch, command_id, *stage)
                    .await
                    .map_err(|e| e.to_string())?;
                info!(deployment = %deployment.id, command = %command_id, ?stage, moved, "v2 link: control acknowledged");
            }
            FromRuntime::PolicyAck { policy_revision } => {
                deployment::record_policy_ack(pool, deployment.id, *policy_revision)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            FromRuntime::RecoveryStatus { ready, missing, .. } => {
                deployment::record_recovery_status(pool, deployment.id, *ready, missing)
                    .await
                    .map_err(|e| e.to_string())?;
                if !ready {
                    warn!(deployment = %deployment.id, missing = ?missing, "v2 link: runtime recovered incompletely");
                }
            }
            FromRuntime::Hello { versions, .. } => {
                info!(deployment = %deployment.id, ?versions, "v2 link: runtime said hello");
            }
            FromRuntime::Events { .. } | FromRuntime::Heartbeat { .. } => {}
        }
        Ok(())
    }

    async fn initial_state(&self, code: &str) -> ToStrategy {
        let Some(s) = self.0.strategies().by_code(code) else {
            return ToStrategy::State { active: false, limits: Default::default(), limits_version: 0, positions: Vec::new(), dead_man_secs: 5 };
        };
        ToStrategy::State {
            active: s.state != TradingState::Halted,
            limits: s.risk_limits(),
            limits_version: s.limits_version.max(0) as u64,
            positions: super::control::positions(&self.0, &s).await,
            dead_man_secs: s.dead_man_secs.into(),
        }
    }
}

/// Serve the control link with the OMS's backend, when configured.
pub async fn serve(state: AppState, settings: ControlLinkSettings) {
    let acceptor = match (std::fs::read(&settings.certificate), std::fs::read(&settings.private_key)) {
        (Ok(cert), Ok(key)) => match tls_acceptor(&cert, &key) {
            Ok(a) => a,
            Err(e) => return tracing::error!(error = %e, "control link: TLS identity unusable; not started"),
        },
        _ => return tracing::error!("control link: certificate or key unreadable; not started"),
    };
    let listener = match TcpListener::bind(("0.0.0.0", settings.port)).await {
        Ok(l) => l,
        Err(e) => return tracing::error!(error = %e, port = settings.port, "control link: cannot listen; not started"),
    };
    info!(port = settings.port, "control link listening");
    let hub = state.colo().clone();
    serve_on(listener, acceptor, Arc::new(AppBackend(state)), hub).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use colo_link::client::{Link, LinkConfig, V2LinkConfig, V2RuntimeLink};
    use colo_link::proto::{FromStrategy, ToStrategy};
    use colo_link::v2::{
        Allocation, EventAck as V2EventAck, EventBatch, EventRecord, FromRuntime,
        FromRuntimeEnvelope, RuntimeCheckpoint, RuntimeLink, Timings, ToRuntime,
    };
    use super::deployment::V2Binding;
    use gateway::runtime::{AccountIdentity, ExecutionCursor, RuntimeEvent, StreamState};

    use super::*;

    struct Stub;

    #[async_trait::async_trait]
    impl LinkBackend for Stub {
        async fn authorize(&self, token: &str, _peer: std::net::SocketAddr) -> Option<String> {
            (token == "tok").then(|| "mm1".to_string())
        }
        async fn initial_state(&self, _code: &str) -> ToStrategy {
            ToStrategy::State { active: true, limits: Default::default(), limits_version: 1, positions: vec![], dead_man_secs: 5 }
        }
    }

    fn deployment() -> Deployment {
        let now = Utc::now();
        Deployment {
            id: Uuid::new_v4(), strategy_code: "mm1".into(), account_id: Uuid::new_v4(),
            venue: "BINANCE".into(), environment: "PAPER".into(), external_account_ref: "sub".into(),
            epoch: 1, status: "ACTIVE".into(), authority_secs: 5, market_data_max_age_secs: 2,
            heartbeat_secs: 1, allocation_quote_asset: "USDT".into(), allocation_max_notional: 1_000.0,
            allocation_max_position_qty: None, policy_revision: 1, kill_epoch: None,
            kill_command_id: None, verified_stop_at: None, verified_stop_epoch: None,
            retired_at: None, shutdown_evidence: None, created_at: now, updated_at: now,
            kill_ack_stage: None, acked_policy_revision: None, recovery_ready: None,
            recovery_missing: None, recovery_reported_at: None,
        }
    }

    struct V2Stub {
        deployment: Deployment,
        account: AccountIdentity,
        nonce: Uuid,
        hub: ColoHub,
        batches: Arc<std::sync::Mutex<Vec<EventBatch>>>,
        runtime_messages: Arc<std::sync::Mutex<Vec<FromRuntime>>>,
    }

    #[async_trait::async_trait]
    impl LinkBackend for V2Stub {
        async fn authorize(&self, _: &str, _: SocketAddr) -> Option<String> { None }
        async fn initial_state(&self, _: &str) -> ToStrategy {
            ToStrategy::State { active: false, limits: Default::default(), limits_version: 0, positions: vec![], dead_man_secs: 5 }
        }
        async fn authorize_v2(&self, token: &str, _: SocketAddr, deployment_id: Uuid, nonce: Uuid, _: bool) -> Option<V2Binding> {
            (token == "tok" && deployment_id == self.deployment.id && nonce == self.nonce).then(|| V2Binding {
                deployment: self.deployment.clone(),
                epoch: 1,
                session_nonce: self.nonce,
                account: self.account.clone(),
                authority_secs: 5,
            })
        }
        async fn bootstrap_v2(&self, _: &Deployment) -> Result<Bootstrap, String> {
            // A valid bootstrap: the runtime's link refuses one without
            // instruments and their limits.
            let rules = gateway::runtime::InstrumentRules {
                symbol: "BTCUSDT".into(), venue: "BINANCE".into(), venue_symbol: "BTCUSDT".into(),
                base_asset: "BTC".into(), quote_asset: "USDT".into(), tick_size: 0.01, step_size: 0.001,
                multiplier: 1.0, min_quantity: None, max_quantity: None, min_notional: None, max_notional: None,
                operations: vec![gateway::runtime::Operation::Place, gateway::runtime::Operation::Cancel],
                capabilities: gateway::runtime::VenueCapabilities {
                    cancel_on_disconnect: false, atomic_cancel_replace: false, open_orders_snapshot: true,
                    order_lookup: true, trade_history: true, balance_snapshot: true,
                },
            };
            Ok(Bootstrap {
                instruments: vec![rules],
                limits: [("BTCUSDT".to_string(), risk::Limits::default())].into_iter().collect(),
                trading_state: risk::TradingState::Active,
                instrument_states: Default::default(), timings: Timings { heartbeat_secs: 1, authority_secs: 5, market_data_max_age_secs: 2 },
                baseline: RuntimeCheckpoint { sequence: 0, positions: Default::default(), open_client_ids: vec![], execution_cursor: ExecutionCursor::default(), complete: true },
                asset_baseline: Default::default(), killed_epoch: None,
                allocation: Allocation { quote_asset: "USDT".into(), max_notional: 1_000.0, max_position_qty: None },
                recovery_universe: vec!["BTCUSDT".into()],
            })
        }
        async fn runtime_message(&self, _: &Deployment, message: &FromRuntime) -> Result<(), String> {
            self.runtime_messages.lock().unwrap().push(message.clone());
            Ok(())
        }
        async fn accept_events(&self, deployment: &Deployment, batch: &EventBatch) -> Result<u64, String> {
            self.batches.lock().unwrap().push(batch.clone());
            let through = batch.last_sequence();
            self.hub.push_v2_control(deployment.id, ToRuntime::EventAck { ack: V2EventAck { through_sequence: through } });
            Ok(through)
        }
    }

    const CERT: &[u8] = include_bytes!("../../crates/colo-link/testdata/cert.pem");
    const KEY: &[u8] = include_bytes!("../../crates/colo-link/testdata/key.pem");

    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_gets_the_state_first_and_acks_are_recorded() {
        let hub = crate::colo::ColoHub::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tls_acceptor(CERT, KEY).unwrap();
        tokio::spawn(serve_on(listener, acceptor, Arc::new(Stub), hub.clone()));

        let (link, mut rx) = Link::spawn(LinkConfig { url: format!("wss://localhost:{port}"), token: "tok".into(), ca_pem: Some(CERT.to_vec()) });
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert!(matches!(first, ToStrategy::State { active: true, .. }));
        link.send(FromStrategy::AckLimits { limits_version: 1 }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = hub.link_status("mm1");
        assert!(status.connected);
        assert_eq!(status.acked_version, 1);

        hub.push("mm1", ToStrategy::Kill);
        let next = loop {
            let m = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
            if m != ToStrategy::Heartbeat {
                break m;
            }
        };
        assert_eq!(next, ToStrategy::Kill);
    }

    #[test]
    fn a_link_silent_for_3_heartbeats_is_dropped() {
        let t0 = std::time::Instant::now();
        assert!(!silent(t0, t0 + Duration::from_millis(2900)));
        assert!(silent(t0, t0 + Duration::from_millis(3100)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_token_is_refused() {
        let hub = crate::colo::ColoHub::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_on(listener, tls_acceptor(CERT, KEY).unwrap(), Arc::new(Stub), hub.clone()));
        let (_link, mut rx) = Link::spawn(LinkConfig { url: format!("wss://localhost:{port}"), token: "nope".into(), ca_pem: Some(CERT.to_vec()) });
        assert!(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.is_err(), "a wrong token got a state");
        assert!(!hub.link_status("mm1").connected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_tls_v2_sockets_bind_bootstrap_events_and_ack() {
        let hub = crate::colo::ColoHub::default();
        let deployment = deployment();
        let nonce = Uuid::new_v4();
        let account = AccountIdentity {
            account_id: deployment.account_id,
            venue: "BINANCE".into(),
            environment: "PAPER".into(),
            external_account_ref: "sub".into(),
        };
        let backend = Arc::new(V2Stub {
            deployment: deployment.clone(),
            account: account.clone(),
            nonce,
            hub: hub.clone(),
            batches: Arc::new(std::sync::Mutex::new(Vec::new())),
            runtime_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_on(listener, tls_acceptor(CERT, KEY).unwrap(), backend.clone(), hub));
        let link = V2RuntimeLink::spawn(V2LinkConfig {
            control_url: format!("wss://localhost:{port}/v2/control?deployment={}&nonce={nonce}", deployment.id),
            events_url: format!("wss://localhost:{port}/v2/events?deployment={}&nonce={nonce}", deployment.id),
            token: "tok".into(),
            ca_pem: Some(CERT.to_vec()),
            account: account.clone(),
            epoch: 1,
            session_nonce: nonce.to_string(),
            policy_revision: 1,
        });
        let first = tokio::time::timeout(Duration::from_secs(5), link.recv_control()).await.unwrap().unwrap();
        assert!(matches!(first.message, ToRuntime::Bootstrap { .. }));

        let batch = EventBatch {
            first_sequence: 1,
            records: vec![EventRecord {
                sequence: 1,
                event: RuntimeEvent::Health { account: account.clone(), state: StreamState::Live, observed_at: Utc::now() },
            }],
        };
        let account_for_ack = account.clone();
        link.send_events(FromRuntimeEnvelope {
            protocol_version: 2,
            account,
            epoch: 1,
            policy_revision: 1,
            sequence: 1,
            session_nonce: nonce.to_string(),
            message: FromRuntime::Events { batch },
        })
        .await
        .unwrap();
        let mut saw_authority = false;
        let ack = loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), link.recv_control()).await.unwrap().unwrap();
            match msg.message {
                ToRuntime::Authority { .. } => saw_authority = true,
                ToRuntime::EventAck { ack } => break ack,
                _ => {}
            }
        };
        assert!(saw_authority, "control socket never delivered authority");
        assert_eq!(ack.through_sequence, 1);
        assert_eq!(backend.batches.lock().unwrap().len(), 1);

        // Acknowledgements and status reach the backend, not just events.
        let envelope = |message| FromRuntimeEnvelope {
            protocol_version: 2,
            account: account_for_ack.clone(),
            epoch: 1,
            policy_revision: 1,
            sequence: 1,
            session_nonce: nonce.to_string(),
            message,
        };
        link.send_events(envelope(FromRuntime::ControlAck {
            epoch: 1,
            command_id: "kill-1".into(),
            stage: colo_link::v2::ControlAckStage::CancelsSent,
        }))
        .await
        .unwrap();
        link.send_events(envelope(FromRuntime::PolicyAck { policy_revision: 1 })).await.unwrap();
        for _ in 0..200 {
            if backend.runtime_messages.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let seen = backend.runtime_messages.lock().unwrap().clone();
        assert!(seen.iter().any(|m| matches!(m, FromRuntime::ControlAck { command_id, .. } if command_id == "kill-1")));
        assert!(seen.iter().any(|m| matches!(m, FromRuntime::PolicyAck { policy_revision: 1 })));
    }
}
