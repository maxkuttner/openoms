//! The control link's OMS side: a TLS WebSocket on its own port. A
//! co-located strategy logs in with its trading token — Kill does not cut
//! this — gets its full state first, then pushes; it acknowledges limits and
//! kills and heartbeats every second.

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use colo_link::proto::{FromStrategy, ToStrategy, HEARTBEAT};

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
    let ws = tokio_tungstenite::accept_hdr_async(tls, |req: &Request, res: Response| {
        if let Some(v) = req.headers().get("Authorization").and_then(|v| v.to_str().ok()) {
            token = v.strip_prefix("Bearer ").unwrap_or("").to_string();
        }
        Ok(res)
    })
    .await?;
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

    use colo_link::client::{Link, LinkConfig};
    use colo_link::proto::{FromStrategy, ToStrategy};

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
}
