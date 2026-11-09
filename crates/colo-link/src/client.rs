//! The strategy's side of the link: a client that keeps reconnecting, and the
//! dead man's switch that decides whether the strategy may trade.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tracing::{info, warn};

use gateway::runtime::AccountIdentity;

use crate::proto::{FromStrategy, ToStrategy, HEARTBEAT};
use crate::v2::{
    ControlFrame, ControlGate, FromRuntime, FromRuntimeEnvelope, ProtocolError, Received, RuntimeLink, ToRuntime,
    ToRuntimeEnvelope, PROTOCOL_VERSION,
};

const V2_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the strategy may trade: it has been told it is active, it is not
/// killed, and it heard the OMS within the timeout. The OMS's `State` (sent
/// on every connect) is authoritative: it sets the timeout, and an active
/// state lifts a kill the strategy missed the `Resume` of.
pub struct DeadMan {
    timeout: Duration,
    last_heard: Instant,
    active: bool,
    killed: bool,
}

impl DeadMan {
    /// Starts stopped: nothing trades before the OMS's first `State`.
    pub fn new(timeout: Duration, now: Instant) -> Self {
        Self { timeout, last_heard: now, active: false, killed: false }
    }

    /// Any message from the OMS, heartbeats included.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
    }

    pub fn apply(&mut self, msg: &ToStrategy) {
        match msg {
            ToStrategy::State { active, dead_man_secs, .. } => {
                self.active = *active;
                self.killed = !*active;
                self.timeout = Duration::from_secs(*dead_man_secs);
            }
            ToStrategy::Kill => self.killed = true,
            ToStrategy::Resume => {
                self.killed = false;
                self.active = true;
            }
            _ => {}
        }
    }

    pub fn may_trade(&self, now: Instant) -> bool {
        self.active && !self.killed && now.saturating_duration_since(self.last_heard) <= self.timeout
    }
}

pub struct LinkConfig {
    /// `wss://host:port`.
    pub url: String,
    /// The strategy's trading token, sent as `Authorization: Bearer`.
    pub token: String,
    /// An extra trusted root (the OMS's own certificate, if self-signed).
    pub ca_pem: Option<Vec<u8>>,
}

/// The sending half of a running link.
#[derive(Clone)]
pub struct Link {
    out: mpsc::Sender<FromStrategy>,
}

impl Link {
    /// Connect, and reconnect a second after any drop, for the life of the
    /// process. Heartbeats every second. Every message from the OMS arrives on
    /// the returned channel.
    pub fn spawn(cfg: LinkConfig) -> (Link, mpsc::Receiver<ToStrategy>) {
        let (out_tx, mut out_rx) = mpsc::channel::<FromStrategy>(64);
        let (in_tx, in_rx) = mpsc::channel::<ToStrategy>(256);
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_once(&cfg, &mut out_rx, &in_tx).await {
                    warn!(error = %e, "control link: down; reconnecting");
                }
                if in_tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        (Link { out: out_tx }, in_rx)
    }

    pub async fn send(&self, msg: FromStrategy) {
        let _ = self.out.send(msg).await;
    }
}

async fn run_once(
    cfg: &LinkConfig,
    out_rx: &mut mpsc::Receiver<FromStrategy>,
    in_tx: &mpsc::Sender<ToStrategy>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut tls = native_tls::TlsConnector::builder();
    if let Some(pem) = &cfg.ca_pem {
        tls.add_root_certificate(native_tls::Certificate::from_pem(pem)?);
    }
    let mut request = cfg.url.as_str().into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", cfg.token).parse()?);
    let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(tokio_tungstenite::Connector::NativeTls(tls.build()?)),
    )
    .await?;
    info!(url = %cfg.url, "control link: connected");
    let (mut sink, mut stream) = ws.split();
    let mut beat = tokio::time::interval(HEARTBEAT);
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ToStrategy>(&text) {
                    Ok(msg) => { if in_tx.send(msg).await.is_err() { return Ok(()); } }
                    Err(e) => warn!(error = %e, "control link: unreadable message"),
                },
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            msg = out_rx.recv() => match msg {
                Some(msg) => sink.send(Message::Text(serde_json::to_string(&msg)?)).await?,
                None => return Ok(()),
            },
            _ = beat.tick() => sink.send(Message::Text(serde_json::to_string(&FromStrategy::Heartbeat)?)).await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
use std::time::{Duration, Instant};

    use super::*;
    use crate::proto::ToStrategy;

    fn state(active: bool) -> ToStrategy {
        ToStrategy::State { active, limits: Default::default(), limits_version: 1, positions: vec![], dead_man_secs: 5 }
    }

    #[test]
    fn the_switch_trips_after_the_timeout_and_rearms_on_active() {
        let t0 = Instant::now();
        let mut d = DeadMan::new(Duration::from_secs(5), t0);
        d.heard(t0);
        d.apply(&state(true));
        assert!(d.may_trade(t0 + Duration::from_millis(4900)));
        assert!(!d.may_trade(t0 + Duration::from_millis(5100)));
        let t1 = t0 + Duration::from_secs(8);
        d.heard(t1);
        d.apply(&state(true));
        assert!(d.may_trade(t1));
        d.apply(&state(false));
        assert!(!d.may_trade(t1));
    }

    #[test]
    fn kill_holds_until_resume_or_an_active_state() {
        let t0 = Instant::now();
        let mut d = DeadMan::new(Duration::from_secs(5), t0);
        d.heard(t0);
        d.apply(&state(true));
        d.apply(&ToStrategy::Kill);
        assert!(!d.may_trade(t0));
        d.apply(&ToStrategy::Resume);
        assert!(d.may_trade(t0));
        d.apply(&ToStrategy::Kill);
        d.apply(&state(true));
        assert!(d.may_trade(t0), "enabled while disconnected: the reconnect's state lifts the kill");
    }

    #[test]
    fn the_oms_sets_the_timeout() {
        let t0 = Instant::now();
        let mut d = DeadMan::new(Duration::from_secs(5), t0);
        d.heard(t0);
        d.apply(&ToStrategy::State { active: true, limits: Default::default(), limits_version: 1, positions: vec![], dead_man_secs: 2 });
        assert!(d.may_trade(t0 + Duration::from_millis(1900)));
        assert!(!d.may_trade(t0 + Duration::from_millis(2100)));
    }

    #[test]
    fn nothing_trades_before_the_first_state() {
        let t0 = Instant::now();
        let d = DeadMan::new(Duration::from_secs(5), t0);
        assert!(!d.may_trade(t0));
    }
}


/// Configuration for the two-connection v2 link: control carries bootstrap,
/// policy, authority and kills; events carries bounded batches and ACKs.
#[derive(Clone)]
pub struct V2LinkConfig {
    pub control_url: String,
    pub events_url: String,
    pub token: String,
    pub ca_pem: Option<Vec<u8>>,
    pub account: AccountIdentity,
    pub epoch: u64,
    pub session_nonce: String,
    pub policy_revision: u64,
}

/// The runtime's sending half. The control and event queues are separate, so a
/// delayed event write cannot hold up a kill.
#[derive(Clone)]
pub struct V2Link {
    control: mpsc::Sender<FromRuntimeEnvelope>,
    events: mpsc::Sender<FromRuntimeEnvelope>,
    account: AccountIdentity,
    epoch: u64,
    policy_revision: u64,
    session_nonce: String,
}

impl V2Link {
    pub fn spawn(
        cfg: V2LinkConfig,
    ) -> (
        V2Link,
        mpsc::Receiver<ControlFrame>,
        mpsc::Receiver<ControlFrame>,
    ) {
        let (control_tx, control_rx) = mpsc::channel::<FromRuntimeEnvelope>(256);
        let (events_tx, events_rx) = mpsc::channel::<FromRuntimeEnvelope>(256);
        let (control_in_tx, control_in_rx) = mpsc::channel::<ControlFrame>(256);
        let (events_in_tx, events_in_rx) = mpsc::channel::<ControlFrame>(256);

        let control_cfg = cfg.clone();
        tokio::spawn(async move {
            run_v2(&control_cfg, control_cfg.control_url.clone(), control_rx, control_in_tx).await;
        });
        let event_cfg = cfg.clone();
        tokio::spawn(async move {
            run_v2(&event_cfg, event_cfg.events_url.clone(), events_rx, events_in_tx).await;
        });

        (
            V2Link {
                control: control_tx,
                events: events_tx,
                account: cfg.account,
                epoch: cfg.epoch,
                policy_revision: cfg.policy_revision,
                session_nonce: cfg.session_nonce,
            },
            control_in_rx,
            events_in_rx,
        )
    }

    pub async fn send_control(&self, message: FromRuntime) -> Result<(), ()> {
        self.control.send(self.envelope(message)).await.map_err(|_| ())
    }

    pub async fn send_events(&self, message: FromRuntime) -> Result<(), ()> {
        self.events.send(self.envelope(message)).await.map_err(|_| ())
    }

    pub async fn send_control_envelope(&self, envelope: FromRuntimeEnvelope) -> Result<(), ()> {
        self.control.send(envelope).await.map_err(|_| ())
    }

    pub async fn send_events_envelope(&self, envelope: FromRuntimeEnvelope) -> Result<(), ()> {
        self.events.send(envelope).await.map_err(|_| ())
    }

    fn envelope(&self, message: FromRuntime) -> FromRuntimeEnvelope {
        FromRuntimeEnvelope {
            protocol_version: PROTOCOL_VERSION,
            account: self.account.clone(),
            epoch: self.epoch,
            policy_revision: self.policy_revision,
            sequence: 0,
            session_nonce: self.session_nonce.clone(),
            message,
        }
    }
}

async fn run_v2(
    cfg: &V2LinkConfig,
    url: String,
    mut out_rx: mpsc::Receiver<FromRuntimeEnvelope>,
    in_tx: mpsc::Sender<ControlFrame>,
) {
    let mut connection = 0;
    loop {
        if let Err(e) = run_v2_once(cfg, &url, &mut out_rx, &in_tx, &mut connection).await {
            warn!(error = %e, "v2 link: down; reconnecting");
        }
        if in_tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn run_v2_once(
    cfg: &V2LinkConfig,
    url: &str,
    out_rx: &mut mpsc::Receiver<FromRuntimeEnvelope>,
    in_tx: &mpsc::Sender<ControlFrame>,
    connection: &mut u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut tls = native_tls::TlsConnector::builder();
    if let Some(pem) = &cfg.ca_pem {
        tls.add_root_certificate(native_tls::Certificate::from_pem(pem)?);
    }
    let mut request = url.into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", cfg.token).parse()?);
    let (ws, _) = tokio::time::timeout(
        V2_HANDSHAKE_TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tokio_tungstenite::Connector::NativeTls(tls.build()?)),
        ),
    )
    .await
    .map_err(|_| "v2 handshake timed out")??;
    *connection += 1;
    let connection = *connection;
    info!(url, connection, "v2 link: connected");
    let (mut sink, mut stream) = ws.split();
    let mut beat = tokio::time::interval(HEARTBEAT);
    loop {
        tokio::select! {
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ToRuntimeEnvelope>(&text) {
                    Ok(envelope) => {
                        let frame = ControlFrame { connection, received_at: Instant::now(), envelope };
                        if in_tx.send(frame).await.is_err() { return Ok(()); }
                    }
                    Err(e) => warn!(error = %e, "v2 link: unreadable message"),
                },
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            msg = out_rx.recv() => match msg {
                Some(msg) => sink.send(Message::Text(serde_json::to_string(&msg)?)).await?,
                None => return Ok(()),
            },
            _ = beat.tick() => {
                let heartbeat = FromRuntimeEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    account: cfg.account.clone(),
                    epoch: cfg.epoch,
                    policy_revision: cfg.policy_revision,
                    sequence: 0,
                    session_nonce: cfg.session_nonce.clone(),
                    message: FromRuntime::Heartbeat { last_event_sequence: 0 },
                };
                sink.send(Message::Text(serde_json::to_string(&heartbeat)?)).await?;
            },
        }
    }
}


/// Adapter from the two-socket v2 transport to the runtime's transport seam.
pub struct V2RuntimeLink {
    link: V2Link,
    /// The receiver and the gate that validates what comes out of it, held
    /// together so frames are admitted in arrival order.
    control: tokio::sync::Mutex<(mpsc::Receiver<ControlFrame>, ControlGate)>,
}

impl V2RuntimeLink {
    pub fn spawn(cfg: V2LinkConfig) -> Arc<Self> {
        let gate = ControlGate::new(cfg.account.clone(), cfg.epoch, cfg.session_nonce.clone());
        let (link, control, mut events) = V2Link::spawn(cfg);
        tokio::spawn(async move { while events.recv().await.is_some() {} });
        Arc::new(Self { link, control: tokio::sync::Mutex::new((control, gate)) })
    }
}

#[async_trait::async_trait]
impl RuntimeLink for V2RuntimeLink {
    async fn recv_control(&self) -> Option<Received<ToRuntime>> {
        let mut guard = self.control.lock().await;
        let (frames, gate) = &mut *guard;
        loop {
            let frame = frames.recv().await?;
            let (connection, sequence) = (frame.connection, frame.envelope.sequence);
            match gate.admit(frame) {
                Ok(received) => return Some(received),
                Err(e) => tracing::warn!(connection, sequence, error = %e, "v2 runtime link: control message dropped"),
            }
        }
    }

    async fn send_events(&self, envelope: FromRuntimeEnvelope) -> Result<(), ProtocolError> {
        self.link
            .send_events_envelope(envelope)
            .await
            .map_err(|_| ProtocolError::InvalidBatch("v2 event connection closed".into()))
    }
}
