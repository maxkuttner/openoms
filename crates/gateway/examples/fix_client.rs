//! A minimal strategy-side FIX client, for checking the OMS's acceptor end to end.
//!
//! cargo run -p gateway --example fix_client -- HOST:PORT SENDER TARGET PASSWORD SYMBOL VENUE QTY PRICE [SECONDS]
//!
//! Logs on over TLS, sends one limit buy (35=D, GTC), prints every message it
//! gets back, and logs out after SECONDS (default 15).

use std::sync::mpsc;
use std::time::Duration;

use quickfix::{
    Application, ApplicationCallback, ConnectionHandler, Dictionary, FieldMap, FixSocketServerKind, Initiator,
    LogCallback, LogFactory, MemoryMessageStoreFactory, Message, MsgFromAppError, SessionId, SessionSettings,
};

struct Client {
    password: String,
    logged_on: mpsc::Sender<()>,
}

impl ApplicationCallback for Client {
    fn on_logon(&self, _: &SessionId) {
        println!("logged on");
        let _ = self.logged_on.send(());
    }
    fn on_logout(&self, _: &SessionId) {
        println!("logged out");
    }
    fn on_msg_to_admin(&self, msg: &mut Message, _: &SessionId) {
        if msg.with_header(|h| h.get_field(35)).as_deref() == Some("A") {
            let _ = msg.set_field(554, self.password.as_str());
        }
    }
    fn on_msg_from_app(&self, msg: &Message, _: &SessionId) -> Result<(), MsgFromAppError> {
        println!("<- {}", msg.to_fix_string().unwrap_or_default().replace('\u{1}', "|"));
        Ok(())
    }
}

struct Quiet;

impl LogCallback for Quiet {
    fn on_incoming(&self, _: Option<&SessionId>, _: &str) {}
    fn on_outgoing(&self, _: Option<&SessionId>, _: &str) {}
    fn on_event(&self, _: Option<&SessionId>, msg: &str) {
        if msg.contains("rror") || msg.contains("efused") || msg.contains("isconnect") {
            println!("   {msg}");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 8 {
        eprintln!("usage: fix_client HOST:PORT SENDER TARGET PASSWORD SYMBOL VENUE QTY PRICE [SECONDS]");
        std::process::exit(2);
    }
    let (host, port) = args[0].rsplit_once(':').expect("HOST:PORT");
    let (sender, target, password) = (&args[1], &args[2], args[3].clone());
    let (symbol, venue, qty, price) = (&args[4], &args[5], &args[6], &args[7]);
    let seconds: u64 = args.get(8).and_then(|s| s.parse().ok()).unwrap_or(15);

    let mut settings = SessionSettings::new();
    let mut d = Dictionary::new();
    d.set("ConnectionType", "initiator".to_string()).unwrap();
    d.set("SocketConnectHost", host.to_string()).unwrap();
    d.set("SocketConnectPort", port.parse::<i32>().expect("port")).unwrap();
    d.set("HeartBtInt", 30i32).unwrap();
    d.set("ReconnectInterval", 5i32).unwrap();
    d.set("StartTime", "00:00:00".to_string()).unwrap();
    d.set("EndTime", "00:00:00".to_string()).unwrap();
    d.set("UseDataDictionary", "N".to_string()).unwrap();
    d.set("ResetOnLogon", "Y".to_string()).unwrap();
    settings.set(None, d).unwrap();
    let sid = SessionId::try_new("FIX.4.4", sender, target, "").unwrap();
    let mut s = Dictionary::new();
    s.set("BeginString", "FIX.4.4".to_string()).unwrap();
    s.set("SenderCompID", sender.clone()).unwrap();
    s.set("TargetCompID", target.clone()).unwrap();
    settings.set(Some(&sid), s).unwrap();

    let (logged_on, on_logon) = mpsc::channel();
    let cb = Client { password, logged_on };
    let app = Application::try_new(&cb).unwrap();
    let store = MemoryMessageStoreFactory::new();
    let log = LogFactory::try_new(&Quiet).unwrap();
    let mut init = Initiator::try_new(&settings, &app, &store, &log, FixSocketServerKind::SslSingleThreaded).unwrap();
    init.start().unwrap();

    if on_logon.recv_timeout(Duration::from_secs(10)).is_err() {
        eprintln!("no logon within 10 s (wrong password, CompIDs or address, or the strategy is killed)");
        let _ = init.stop();
        std::process::exit(1);
    }
    let cl_ord_id = format!("e2e-{}", std::process::id());
    let text = format!("8=FIX.4.4|9=1|35=D|11={cl_ord_id}|55={symbol}|100={venue}|54=1|38={qty}|40=2|44={price}|59=1|10=000|");
    let order = Message::try_from_text(&text.replace('|', "\u{1}")).unwrap();
    println!("-> 35=D 11={cl_ord_id} 55={symbol} 100={venue} buy {qty} @ {price}");
    quickfix::send_to_target(order, &sid).unwrap();
    std::thread::sleep(Duration::from_secs(seconds));
    let _ = init.stop();
}
