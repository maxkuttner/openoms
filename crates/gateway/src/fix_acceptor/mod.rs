//! The FIX acceptor strategies / algos log on to (path ①).
//!
//! Speaks FIX 4.4 over TLS and nothing else: requests come out as plain
//! [`ClientRequest`]s and go back in as [`ClientReport`]s, so the core never
//! sees a FIX tag. Who may log on is the core's decision ([`LogonCheck`]); the
//! acceptor holds no database and no registry.

pub mod codec;

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use quickfix::{
    Acceptor, Application, ApplicationCallback, ConnectionHandler, Dictionary, FieldMap, FixSocketServerKind,
    LogFactory, MemoryMessageStoreFactory, Message, MsgFromAdminError, MsgFromAppError, QuickFixError, Session,
    SessionContainer, SessionId, SessionSettings,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// FIX 4.4, the version the OMS already speaks to its FIX venues.
pub const BEGIN_STRING: &str = "FIX.4.4";

/// A strategy's session, named from the strategy's side: its SenderCompID and
/// the CompID it targets (the OMS).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub sender_comp_id: String,
    pub target_comp_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    Day,
    Gtc,
    Ioc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecType {
    New,
    Trade,
    Canceled,
    Rejected,
    Expired,
    OrderStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequest {
    NewOrder {
        cl_ord_id: String,
        symbol: String,
        ex_destination: String,
        side: Side,
        qty: f64,
        ord_type: OrdType,
        price: Option<f64>,
        tif: Tif,
    },
    Cancel {
        cl_ord_id: String,
        orig_cl_ord_id: String,
    },
    Status {
        cl_ord_id: String,
    },
    SecurityList {
        req_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientReport {
    Exec {
        cl_ord_id: String,
        order_id: String,
        exec_id: String,
        exec_type: ExecType,
        ord_status: OrdStatus,
        symbol: String,
        side: Side,
        order_qty: f64,
        cum_qty: f64,
        leaves_qty: f64,
        avg_px: f64,
        last_qty: Option<f64>,
        last_px: Option<f64>,
        text: Option<String>,
    },
    CancelReject {
        cl_ord_id: String,
        orig_cl_ord_id: String,
        text: String,
    },
    /// (ex_destination, symbol, OMS instrument id) per tradeable instrument.
    SecurityList {
        req_id: String,
        securities: Vec<(String, String, i64)>,
    },
    BusinessReject {
        ref_msg_type: String,
        text: String,
    },
}

/// What the acceptor tells the core about a session.
#[derive(Debug)]
pub enum Inbound {
    LoggedOn,
    LoggedOut,
    Request(ClientRequest),
}

/// The core's decision on a Logon: `Err` carries the reason, which is logged
/// (QuickFIX's logon refusal carries no text back to the client).
pub trait LogonCheck: Send + Sync {
    fn check(&self, key: &SessionKey, password: Option<&str>) -> Result<(), String>;
}

pub struct AcceptorConfig {
    pub port: u16,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    /// Every session the acceptor knows, with its allowed remote addresses
    /// (empty: any). QuickFIX has no dynamic sessions, so this list is fixed
    /// until the acceptor is restarted.
    pub sessions: Vec<(SessionKey, Vec<String>)>,
}

enum Cmd {
    Stop(std_mpsc::Sender<usize>),
}

/// A running acceptor. QuickFIX and its callbacks live on their own OS thread
/// (none of its handles are `Send`); this handle reaches them by session id.
pub struct AcceptorHandle {
    cmd: std_mpsc::Sender<Cmd>,
    sessions: Vec<SessionKey>,
}

/// The OMS's own side of a strategy's session: QuickFIX names a session from
/// the side that owns it.
fn oms_session_id(key: &SessionKey) -> Result<SessionId, QuickFixError> {
    SessionId::try_new(BEGIN_STRING, &key.target_comp_id, &key.sender_comp_id, "")
}

fn key_of(sid: &SessionId) -> SessionKey {
    SessionKey {
        sender_comp_id: sid.get_target_comp_id().unwrap_or_default(),
        target_comp_id: sid.get_sender_comp_id().unwrap_or_default(),
    }
}

fn settings(cfg: &AcceptorConfig) -> Result<SessionSettings, QuickFixError> {
    let mut settings = SessionSettings::new();
    let mut d = Dictionary::new();
    d.set("ConnectionType", "acceptor".to_string())?;
    d.set("SocketAcceptPort", cfg.port as i32)?;
    d.set("StartTime", "00:00:00".to_string())?;
    d.set("EndTime", "00:00:00".to_string())?;
    d.set("UseDataDictionary", "N".to_string())?;
    // Sequence numbers restart on each Logon; what a strategy missed is
    // replayed by the core from the event store.
    d.set("ResetOnLogon", "Y".to_string())?;
    d.set("ServerCertificateFile", cfg.certificate.display().to_string())?;
    d.set("ServerCertificateKeyFile", cfg.private_key.display().to_string())?;
    settings.set(None, d)?;
    for (key, allowed) in &cfg.sessions {
        let sid = oms_session_id(key)?;
        let mut s = Dictionary::new();
        s.set("BeginString", BEGIN_STRING.to_string())?;
        s.set("SenderCompID", key.target_comp_id.clone())?;
        s.set("TargetCompID", key.sender_comp_id.clone())?;
        if !allowed.is_empty() {
            s.set("AllowedRemoteAddresses", allowed.join(","))?;
        }
        settings.set(Some(&sid), s)?;
    }
    Ok(settings)
}

struct AcceptorApp {
    logon: Arc<dyn LogonCheck>,
    inbound: mpsc::UnboundedSender<(SessionKey, Inbound)>,
}

impl ApplicationCallback for AcceptorApp {
    fn on_logon(&self, sid: &SessionId) {
        info!(session = ?key_of(sid), "FIX acceptor: logon");
        let _ = self.inbound.send((key_of(sid), Inbound::LoggedOn));
    }

    fn on_logout(&self, sid: &SessionId) {
        info!(session = ?key_of(sid), "FIX acceptor: logout");
        let _ = self.inbound.send((key_of(sid), Inbound::LoggedOut));
    }

    fn on_msg_from_admin(&self, msg: &Message, sid: &SessionId) -> Result<(), MsgFromAdminError> {
        if msg.with_header(|h| h.get_field(35)).as_deref() != Some("A") {
            return Ok(());
        }
        let key = key_of(sid);
        match self.logon.check(&key, msg.get_field(554).as_deref()) {
            Ok(()) => Ok(()),
            Err(reason) => {
                warn!(session = ?key, %reason, "FIX acceptor: logon refused");
                Err(MsgFromAdminError::RejectLogon)
            }
        }
    }

    fn on_msg_from_app(&self, msg: &Message, sid: &SessionId) -> Result<(), MsgFromAppError> {
        match codec::decode(msg) {
            Ok(request) => {
                let _ = self.inbound.send((key_of(sid), Inbound::Request(request)));
            }
            Err(reject) => match codec::encode(&reject) {
                Ok(m) => {
                    if let Err(e) = quickfix::send_to_target(m, sid) {
                        warn!(error = %e, "FIX acceptor: could not send a business reject");
                    }
                }
                Err(e) => warn!(error = %e, "FIX acceptor: could not build a business reject"),
            },
        }
        Ok(())
    }
}

impl AcceptorHandle {
    /// Start listening. Fails if the settings, the certificate or the port are
    /// unusable, rather than leaving a dead thread behind.
    pub fn start(
        cfg: AcceptorConfig,
        logon: Arc<dyn LogonCheck>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<(SessionKey, Inbound)>), String> {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();
        let sessions: Vec<SessionKey> = cfg.sessions.iter().map(|(k, _)| k.clone()).collect();
        let known = sessions.clone();
        std::thread::Builder::new()
            .name("fix-acceptor".into())
            .spawn(move || {
                let fail = |e: String| {
                    let _ = ready_tx.send(Err(e));
                };
                let settings = match settings(&cfg) {
                    Ok(s) => s,
                    Err(e) => return fail(format!("settings: {e}")),
                };
                let cb = AcceptorApp { logon, inbound: inbound_tx };
                let app = match Application::try_new(&cb) {
                    Ok(a) => a,
                    Err(e) => return fail(format!("application: {e}")),
                };
                let store = MemoryMessageStoreFactory::new();
                let log = match LogFactory::try_new(&crate::fix::TracingFixLog) {
                    Ok(l) => l,
                    Err(e) => return fail(format!("log: {e}")),
                };
                let mut acceptor = match Acceptor::try_new(&settings, &app, &store, &log, FixSocketServerKind::SslMultiThreaded) {
                    Ok(a) => a,
                    Err(e) => return fail(format!("acceptor: {e}")),
                };
                if let Err(e) = acceptor.start() {
                    return fail(format!("start: {e}"));
                }
                let _ = ready_tx.send(Ok(()));
                info!(port = cfg.port, sessions = known.len(), "FIX acceptor listening");
                // Parked until stopped; a dropped handle stops it too.
                let reply = match cmd_rx.recv() {
                    Ok(Cmd::Stop(reply)) => Some(reply),
                    Err(_) => None,
                };
                let live = known
                    .iter()
                    .filter(|k| {
                        oms_session_id(k)
                            .ok()
                            .and_then(|sid| acceptor.session(sid).ok())
                            .is_some_and(|mut s| s.is_logged_on().unwrap_or(false))
                    })
                    .count();
                let _ = acceptor.stop();
                info!(logged_out = live, "FIX acceptor stopped");
                if let Some(reply) = reply {
                    let _ = reply.send(live);
                }
            })
            .map_err(|e| format!("thread: {e}"))?;
        ready_rx.recv().map_err(|_| "acceptor thread ended during start".to_string())??;
        Ok((Self { cmd: cmd_tx, sessions }, inbound_rx))
    }

    /// Every session this acceptor was started with.
    pub fn sessions(&self) -> &[SessionKey] {
        &self.sessions
    }

    pub fn send(&self, key: &SessionKey, report: ClientReport) -> Result<(), String> {
        let msg = codec::encode(&report).map_err(|e| e.to_string())?;
        let sid = oms_session_id(key).map_err(|e| e.to_string())?;
        quickfix::send_to_target(msg, &sid).map_err(|e| e.to_string())
    }

    fn with_session(&self, key: &SessionKey, act: impl FnOnce(&mut Session<'static>) -> Result<(), QuickFixError>) {
        let Ok(sid) = oms_session_id(key) else { return };
        // SAFETY: the session lives as long as the acceptor thread, which
        // outlives this handle's calls (it stops only on `stop`).
        match unsafe { Session::lookup(&sid) } {
            Ok(mut session) => {
                if let Err(e) = act(&mut session) {
                    warn!(session = ?key, error = %e, "FIX acceptor: session call failed");
                }
            }
            Err(e) => warn!(session = ?key, error = %e, "FIX acceptor: no such session"),
        }
    }

    /// Log the session out and keep it out until [`AcceptorHandle::enable`].
    /// Only flags the session: QuickFIX's own thread sends the Logout and
    /// disconnects.
    pub fn logout(&self, key: &SessionKey, reason: &str) {
        info!(session = ?key, %reason, "FIX acceptor: logging session out");
        self.with_session(key, |s| s.logout());
    }

    /// Let a logged-out session log on again.
    pub fn enable(&self, key: &SessionKey) {
        info!(session = ?key, "FIX acceptor: session enabled");
        self.with_session(key, |s| s.logon());
    }

    /// Stop listening and log every session out; returns how many were logged on.
    pub fn stop(self) -> usize {
        let (tx, rx) = std_mpsc::channel();
        if self.cmd.send(Cmd::Stop(tx)).is_err() {
            return 0;
        }
        rx.recv().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    use quickfix::{
        Application, ApplicationCallback, ConnectionHandler, Dictionary, FieldMap, FixSocketServerKind, Initiator,
        LogFactory, MemoryMessageStoreFactory, Message, MsgFromAppError, SessionId, SessionSettings,
    };

    use super::*;

    struct Password(&'static str);

    impl LogonCheck for Password {
        fn check(&self, _key: &SessionKey, password: Option<&str>) -> Result<(), String> {
            if password == Some(self.0) { Ok(()) } else { Err("wrong password".into()) }
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// QuickFIX keeps one process-wide session table, so tests running in
    /// parallel each need their own CompIDs.
    fn key(client: &str) -> SessionKey {
        SessionKey { sender_comp_id: client.into(), target_comp_id: "OMS".into() }
    }

    fn start(port: u16, client: &str) -> (AcceptorHandle, tokio::sync::mpsc::UnboundedReceiver<(SessionKey, Inbound)>) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/fix_acceptor/testdata");
        let cfg = AcceptorConfig {
            port,
            certificate: format!("{dir}/cert.pem").into(),
            private_key: format!("{dir}/key.pem").into(),
            sessions: vec![(key(client), Vec::new())],
        };
        AcceptorHandle::start(cfg, std::sync::Arc::new(Password("pw"))).expect("acceptor starts")
    }

    enum Seen {
        Logon,
        App(String),
    }

    /// A strategy's side of the wire: logs on with `password` and reports what it sees.
    struct Client {
        password: &'static str,
        seen: std_mpsc::Sender<Seen>,
    }

    impl ApplicationCallback for Client {
        fn on_logon(&self, _: &SessionId) {
            let _ = self.seen.send(Seen::Logon);
        }
        fn on_msg_to_admin(&self, msg: &mut Message, _: &SessionId) {
            if msg.with_header(|h| h.get_field(35)).as_deref() == Some("A") {
                let _ = msg.set_field(554, self.password);
            }
        }
        fn on_msg_from_app(&self, msg: &Message, _: &SessionId) -> Result<(), MsgFromAppError> {
            let _ = self.seen.send(Seen::App(msg.to_fix_string().unwrap_or_default()));
            Ok(())
        }
    }

    /// Runs a QuickFIX initiator on its own thread for the life of the test.
    fn client(port: u16, client: &'static str, password: &'static str) -> std_mpsc::Receiver<Seen> {
        let (tx, rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let mut settings = SessionSettings::new();
            let mut d = Dictionary::new();
            d.set("ConnectionType", "initiator".to_string()).unwrap();
            d.set("SocketConnectHost", "127.0.0.1".to_string()).unwrap();
            d.set("SocketConnectPort", port as i32).unwrap();
            d.set("HeartBtInt", 30i32).unwrap();
            d.set("ReconnectInterval", 1i32).unwrap();
            d.set("StartTime", "00:00:00".to_string()).unwrap();
            d.set("EndTime", "00:00:00".to_string()).unwrap();
            d.set("UseDataDictionary", "N".to_string()).unwrap();
            d.set("ResetOnLogon", "Y".to_string()).unwrap();
            settings.set(None, d).unwrap();
            let sid = SessionId::try_new(BEGIN_STRING, client, "OMS", "").unwrap();
            let mut s = Dictionary::new();
            s.set("BeginString", BEGIN_STRING.to_string()).unwrap();
            s.set("SenderCompID", client.to_string()).unwrap();
            s.set("TargetCompID", "OMS".to_string()).unwrap();
            settings.set(Some(&sid), s).unwrap();
            let cb = Client { password, seen: tx };
            let app = Application::try_new(&cb).unwrap();
            let store = MemoryMessageStoreFactory::new();
            let log = LogFactory::try_new(&crate::fix::TracingFixLog).unwrap();
            let mut init = Initiator::try_new(&settings, &app, &store, &log, FixSocketServerKind::SslSingleThreaded).unwrap();
            init.start().unwrap();
            std::thread::sleep(Duration::from_secs(15));
            let _ = init.stop();
        });
        rx
    }

    fn new_order_single() -> Message {
        let text = "8=FIX.4.4|9=1|35=D|11=c1|55=BTCUSDT|100=BINANCE|54=1|38=1|40=2|44=100|59=1|10=000|";
        Message::try_from_text(&text.replace('|', "\u{1}")).unwrap()
    }

    async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(SessionKey, Inbound)>) -> Inbound {
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("nothing from the acceptor").unwrap().1
    }

    /// The phase's "done when", at the protocol level: a QuickFIX initiator logs
    /// on over TLS with the right password, sends 35=D and receives the 35=8 the
    /// OMS sends back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_test_client_logs_on_and_places_an_order() {
        let port = free_port();
        let (acceptor, mut inbound) = start(port, "CLIENT1");
        let seen = client(port, "CLIENT1", "pw");

        assert!(matches!(next(&mut inbound).await, Inbound::LoggedOn));
        assert!(matches!(seen.recv_timeout(Duration::from_secs(5)), Ok(Seen::Logon)));

        let sid = SessionId::try_new(BEGIN_STRING, "CLIENT1", "OMS", "").unwrap();
        quickfix::send_to_target(new_order_single(), &sid).unwrap();
        match next(&mut inbound).await {
            Inbound::Request(ClientRequest::NewOrder { cl_ord_id, ex_destination, .. }) => {
                assert_eq!((cl_ord_id.as_str(), ex_destination.as_str()), ("c1", "BINANCE"));
            }
            _ => panic!("expected the new order"),
        }

        acceptor
            .send(&key("CLIENT1"), ClientReport::Exec {
                cl_ord_id: "c1".into(), order_id: "o1".into(), exec_id: "1".into(),
                exec_type: ExecType::New, ord_status: OrdStatus::New, symbol: "BTCUSDT".into(), side: Side::Buy,
                order_qty: 1.0, cum_qty: 0.0, leaves_qty: 1.0, avg_px: 0.0, last_qty: None, last_px: None, text: None,
            })
            .unwrap();
        match seen.recv_timeout(Duration::from_secs(5)) {
            Ok(Seen::App(m)) => {
                assert!(m.contains("\u{1}35=8\u{1}"), "{m}");
                assert!(m.contains("\u{1}11=c1\u{1}") && m.contains("\u{1}39=0\u{1}"), "{m}");
            }
            _ => panic!("the client got no execution report"),
        }
        assert_eq!(acceptor.stop(), 1);
    }

    /// A logged-out session stays out, however often the client reconnects,
    /// until it is enabled again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_logged_out_session_stays_out_until_enabled() {
        let port = free_port();
        let (acceptor, mut inbound) = start(port, "CLIENT3");
        let _seen = client(port, "CLIENT3", "pw");
        assert!(matches!(next(&mut inbound).await, Inbound::LoggedOn));

        acceptor.logout(&key("CLIENT3"), "killed");
        assert!(matches!(next(&mut inbound).await, Inbound::LoggedOut));
        let again = tokio::time::timeout(Duration::from_secs(3), inbound.recv()).await;
        assert!(again.is_err(), "logged on again while disabled");

        acceptor.enable(&key("CLIENT3"));
        assert!(matches!(next(&mut inbound).await, Inbound::LoggedOn));
        acceptor.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_password_is_refused() {
        let port = free_port();
        let (acceptor, _inbound) = start(port, "CLIENT2");
        let seen = client(port, "CLIENT2", "nope");
        assert!(seen.recv_timeout(Duration::from_secs(3)).is_err(), "logged on with a wrong password");
        acceptor.stop();
    }
}
