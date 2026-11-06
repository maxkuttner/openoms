//! The strategy's side of the link: a client that keeps reconnecting, and the
//! dead man's switch that decides whether the strategy may trade.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tracing::{info, warn};

use crate::proto::{FromStrategy, ToStrategy, HEARTBEAT};

/// Whether the strategy may trade: it has been told it is active, it is not
/// killed, and it heard the OMS within the timeout. A reconnect's `State`
/// re-arms it; only `Resume` lifts a kill.
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
            ToStrategy::State { active, .. } => self.active = *active,
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
    use std::time::{Duration, Instant};

    use super::*;
    use crate::proto::ToStrategy;

    fn state(active: bool) -> ToStrategy {
        ToStrategy::State { active, limits: Default::default(), limits_version: 1, positions: vec![] }
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
    fn kill_holds_until_resume_even_when_heard() {
        let t0 = Instant::now();
        let mut d = DeadMan::new(Duration::from_secs(5), t0);
        d.heard(t0);
        d.apply(&state(true));
        d.apply(&ToStrategy::Kill);
        d.heard(t0);
        assert!(!d.may_trade(t0));
        d.apply(&state(true));
        assert!(!d.may_trade(t0), "a reconnect's state does not undo a kill");
        d.apply(&ToStrategy::Resume);
        assert!(d.may_trade(t0));
    }

    #[test]
    fn nothing_trades_before_the_first_state() {
        let t0 = Instant::now();
        let d = DeadMan::new(Duration::from_secs(5), t0);
        assert!(!d.may_trade(t0));
    }
}
