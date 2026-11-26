pub mod adapter;
pub mod client;
pub mod exchanges;
pub mod gateway;
pub mod probe;
pub mod resolve;
pub mod stream;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;
use serde::Deserialize;

use crate::connectors::RegistrationDeps;
use crate::credentials::BrokerCredentials;
use crate::credentials::Shown::{Clear, Hidden};
use crate::venue::{environments, field, Venue, DOCS};
use crate::verify::TestOutcome;

use super::{typed, Connected, Connector, Manifest, QuoteScope};

pub struct IbkrTws;

/// Connected clients by connection code (`IBKR_TWS-PAPER`), set by the
/// supervisor while a session is up. How the admin API (contract search and
/// import) reaches IB Gateway without owning the connection.
static LIVE: Mutex<Option<HashMap<String, Arc<dyn client::TwsClient>>>> = Mutex::new(None);

/// The connection code the supervisor registers its client under.
pub fn connection_code(env: &str) -> String {
    format!("{}-{env}", MANIFEST.code)
}

/// The connected client for `connection_code`, if its session is up.
pub fn live_client(connection_code: &str) -> Option<Arc<dyn client::TwsClient>> {
    LIVE.lock().unwrap().as_ref()?.get(connection_code).filter(|c| c.connected()).cloned()
}

pub(crate) fn set_live_client(connection_code: &str, client: Option<Arc<dyn client::TwsClient>>) {
    let mut live = LIVE.lock().unwrap();
    let map = live.get_or_insert_with(HashMap::new);
    match client {
        Some(c) => map.insert(connection_code.to_string(), c),
        None => map.remove(connection_code),
    };
}

/// Connection codes last connected to an external gateway, with its
/// `host:port`, so removing one leaves Docker alone (`on_removed` gets no
/// credential) and a credential test knows which gateway the live client is on.
static EXTERNAL: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn set_external(connection_code: &str, addr: Option<String>) {
    let mut map = EXTERNAL.lock().unwrap_or_else(|e| e.into_inner());
    match addr {
        Some(a) => map.insert(connection_code.to_string(), a),
        None => map.remove(connection_code),
    };
}

/// The external gateway `connection_code` was last connected to; `None` = managed.
fn external_addr(connection_code: &str) -> Option<String> {
    EXTERNAL.lock().unwrap_or_else(|e| e.into_inner()).get(connection_code).cloned()
}

fn is_external(connection_code: &str) -> bool {
    external_addr(connection_code).is_some()
}

/// An IBKR login for the IB Gateway container, or the address of an IB
/// Gateway run as its own service (then the login lives there).
/// Stored as `{"kind":"IbkrTws", …}` (see [`crate::credentials::BrokerCredentials`]).
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct IbkrTwsCredentials {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// `host:port` of an IB Gateway the OMS does not start (e.g. a compose
    /// service); `None` (or empty) = the OMS runs it in Docker on 127.0.0.1.
    #[serde(default, deserialize_with = "gateway_address", skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// The TWS API client id the OMS connects as (default 0). Client 0 with
    /// `reqAutoOpenOrders(true)` also sees orders typed into TWS, so they are
    /// adopted. Orders and executions of *other API clients* reach only the
    /// client whose id equals IB Gateway's "Master API client ID" setting
    /// (Configure > API > Settings), which the OMS does not set: set it there
    /// by hand to this value to see them.
    #[serde(default, deserialize_with = "client_id")]
    pub master_client_id: i32,
}

/// A gateway address trimmed; empty or whitespace = none.
fn gateway_address<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<String>::deserialize(d)?;
    Ok(v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

impl IbkrTwsCredentials {
    /// The external gateway's `(host, port)`, or `None` for an OMS-managed one.
    /// Errors never echo the address.
    pub fn external(&self) -> Result<Option<(String, u16)>, String> {
        let Some(addr) = self.gateway.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
            return Ok(None);
        };
        let bad = || "gateway address must be host:port, e.g. ib-gateway:4004".to_string();
        let (host, port) = addr.rsplit_once(':').ok_or_else(bad)?;
        // An IPv6 host needs brackets ([::1]:4004), or its port is ambiguous.
        let bracketed = host.starts_with('[') && host.ends_with(']');
        if host.is_empty() || host.chars().any(char::is_whitespace) || (host.contains(':') && !bracketed) {
            return Err(bad());
        }
        match port.parse::<u16>() {
            Ok(p) if p > 0 => Ok(Some((host.to_string(), p))),
            _ => Err("gateway port must be a number from 1 to 65535".into()),
        }
    }

    /// An external gateway's port is the gnzsnz image's port for this
    /// environment's trading mode, when it is one of the two: a PAPER
    /// connection on the live port (or the reverse) is refused. Another port
    /// passes; the accounts check on connect still guards it.
    pub fn check_environment(&self, env: &str) -> Result<(), String> {
        use gateway::{CONTAINER_PORT_LIVE as LIVE, CONTAINER_PORT_PAPER as PAPER};
        let Some((_, port)) = self.external()? else { return Ok(()) };
        match (env.eq_ignore_ascii_case("live"), port) {
            (false, LIVE) => Err(format!("port {LIVE} is IB Gateway's live port; a PAPER connection uses {PAPER}")),
            (true, PAPER) => Err(format!("port {PAPER} is IB Gateway's paper port; a LIVE connection uses {LIVE}")),
            _ => Ok(()),
        }
    }

    /// Complete enough to connect: a valid gateway address, or else a login
    /// for the gateway the OMS starts.
    pub fn check(&self) -> Result<(), String> {
        if self.external()?.is_some() {
            return Ok(());
        }
        if self.username.trim().is_empty() || self.password.is_empty() {
            return Err("username and password are required unless a gateway address is set".into());
        }
        Ok(())
    }
}

/// Save-time check for [`Manifest::validate`]: names the field to fix.
fn validate(creds: &BrokerCredentials, env: &str) -> Result<(), (&'static str, String)> {
    // Every other field is text, so only the client id can fail to decode;
    // serde's message would echo the value, so it is not passed on.
    let c: IbkrTwsCredentials = creds.parse().map_err(|_| ("master_client_id", "not a whole number".to_string()))?;
    if c.gateway.is_some() {
        return c.check().and_then(|()| c.check_environment(env)).map_err(|why| ("gateway", why));
    }
    let field = if c.username.trim().is_empty() { "username" } else { "password" };
    c.check().map_err(|why| (field, why))
}

/// A client id stored as a number or as the form's text (empty = 0).
fn client_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    use serde::de::Error;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or_else(|| D::Error::custom("master_client_id out of range")),
        serde_json::Value::String(s) if s.trim().is_empty() => Ok(0),
        serde_json::Value::String(s) => s.trim().parse().map_err(|_| D::Error::custom(format!("master_client_id is not a number: {s}"))),
        serde_json::Value::Null => Ok(0),
        other => Err(D::Error::custom(format!("master_client_id is not a number: {other}"))),
    }
}

static MANIFEST: Manifest = Manifest {
    code: "IBKR_TWS",
    credential_kind: "IbkrTws",
    venue,
    field_types: &[],
    redaction: &[("username", Clear), ("password", Hidden), ("gateway", Clear), ("master_client_id", Clear)],
    live_only: false,
    universe: false,
    quotes: QuoteScope::NonCrypto,
    exec_stream: true,
    session_bound: false,
    price_band_exempt: true,
    per_currency_notional: true,
    multi_account: true,
    instrument_classes: &["SPOT","FUTURE","OPTION"],
    validate,
};

impl Connector for IbkrTws {
    fn manifest(&self) -> &'static Manifest {
        &MANIFEST
    }

    /// Builds the adapter now; the execution stream owns the gateway: it starts
    /// or adopts the container, connects, and hands the client to the adapter.
    fn connect(&self, env: &'static str, creds: &BrokerCredentials, _: &RegistrationDeps) -> Result<Connected, String> {
        let creds: IbkrTwsCredentials = typed(creds, MANIFEST.credential_kind)?;
        creds.check()?;
        creds.check_environment(env)?;
        let external = creds.external()?.map(|(host, port)| format!("{host}:{port}"));
        set_external(&connection_code(env), external.clone());
        let external = external.is_some();
        // An external gateway needs no env file: no state dir either.
        let state_dir = if external {
            PathBuf::new()
        } else {
            state_dir().ok_or("no HOME or XDG_STATE_HOME to keep the IB Gateway env file under")?
        };
        let slot: adapter::ClientSlot = Arc::new(RwLock::new(None));
        let adapter = Arc::new(adapter::IbkrTwsAdapter::new(slot.clone()));
        let stream_adapter = adapter.clone();
        // Venue events only: the reconciler books every fill (exactly once,
        // through ingest) and pulls what the stream missed.
        Ok(Connected {
            source: Some(adapter.clone()),
            daily_stats: Some(adapter.clone()),
            adapter,
            venue_stream: Some(Arc::new(move |health, tx| {
                let gw = stream::Gateway {
                    docker: Arc::new(gateway::DockerCli),
                    code: format!("{}-{env}", MANIFEST.code),
                    env,
                    creds: creds.clone(),
                    state_dir: state_dir.clone(),
                };
                let ctx = stream::Ctx { adapter: stream_adapter.clone(), health, tx };
                Box::pin(stream::supervise(gw, slot.clone(), ctx))
            })),
        })
    }

    fn universe_provider(&self, _: &str, _: &BrokerCredentials) -> Result<Box<dyn crate::adapter::InstrumentProvider>, String> {
        Err("IBKR instruments are imported from a CSV or search".into())
    }

    /// Says whether IB Gateway is up and logged in, and whether the OMS can
    /// trade through it: the running session if there is one (on the same
    /// gateway), else a short probe of the external address, else the state
    /// of the OMS's own container. Never starts a container. See [`probe`].
    fn verify<'a>(&'a self, env: &'a str, creds: &'a BrokerCredentials) -> BoxFuture<'a, TestOutcome> {
        Box::pin(async move {
            let creds: IbkrTwsCredentials = match typed(creds, MANIFEST.credential_kind) {
                Ok(c) => c,
                Err(e) => return TestOutcome::Failed(e),
            };
            let code = connection_code(env);
            let addr = creds.external().ok().flatten().map(|(host, port)| format!("{host}:{port}"));
            // The session's client only speaks for these credentials when it
            // is on the same gateway (an edited address is probed afresh).
            let live = live_client(&code).filter(|_| external_addr(&code) == addr);
            let target = probe::Target { env, code: &code, creds: &creds, live, docker: Arc::new(gateway::DockerCli) };
            probe::test(target, crate::verify::TEST_TIMEOUT, |addr, client_id| async move {
                client::IbapiClient::connect(&addr, client_id).await.map(|c| Arc::new(c) as Arc<dyn client::TwsClient>)
            })
            .await
        })
    }

    /// Stops and removes the connection's IB Gateway container; nothing for
    /// an external gateway, which the OMS neither started nor may stop.
    fn on_removed(&self, env: &str) -> BoxFuture<'static, ()> {
        let code = connection_code(env);
        if is_external(&code) {
            return Box::pin(async {});
        }
        Box::pin(gateway::remove(Arc::new(gateway::DockerCli), code))
    }
}

/// `$XDG_STATE_HOME/oms/ibkr`, else `~/.local/state/oms/ibkr`: where the
/// short-lived gateway env file is written (mode 0600, deleted after `docker run`).
fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("oms").join("ibkr"))
}

fn venue() -> Venue {
    Venue {
        code: "IBKR_TWS",
        name: "Interactive Brokers (TWS API)",
        description: "Stocks, futures and options via IB Gateway",
        docs_url: format!("{DOCS}/interactive-brokers/"),
        // No catalog to load: instruments are imported from a CSV or search,
        // and futures sync on their own. `None` keeps the "load now?" prompt away.
        universe: None,
        environments: environments(
            "paper trading account",
            Some("https://www.interactivebrokers.com/portal/"),
            Some("https://www.interactivebrokers.com/portal/"),
        ),
        fields: vec![
            field("username", "IBKR username").optional(),
            field("password", "Password").secret().optional(),
            field("gateway", "Gateway address (optional): host:port of an IB Gateway you run yourself, e.g. ib-gateway:4004 (paper) or :4003 (live). Leave empty to let the OMS start it with Docker.").optional(),
            field("master_client_id", "API client id (0 also sees orders entered in TWS; to see other API clients' orders set IB Gateway's Master API client ID to this value)").prefill("0", "0"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_comes_from_credentials() {
        let parse = |v: serde_json::Value| serde_json::from_value::<IbkrTwsCredentials>(v).map(|c| c.master_client_id);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p"})).unwrap(), 0, "default: master");
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": 7})).unwrap(), 7);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": "12"})).unwrap(), 12);
        assert_eq!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": ""})).unwrap(), 0);
        assert!(parse(serde_json::json!({"username": "u", "password": "p", "master_client_id": "x"})).is_err());
    }

    fn creds(username: &str, password: &str, gateway: &str) -> IbkrTwsCredentials {
        serde_json::from_value(serde_json::json!({"username": username, "password": password, "gateway": gateway})).unwrap()
    }

    #[test]
    fn gateway_address_parses() {
        let ext = |g: &str| creds("", "", g).external();
        assert_eq!(ext("ib-gateway:4004").unwrap(), Some(("ib-gateway".into(), 4004)));
        assert_eq!(ext(" ib-gateway:4004 ").unwrap(), Some(("ib-gateway".into(), 4004)), "trimmed");
        assert_eq!(ext("").unwrap(), None);
        assert_eq!(ext("   ").unwrap(), None);
        assert_eq!(ext("[::1]:4004").unwrap(), Some(("[::1]".into(), 4004)), "bracketed IPv6");
        for bad in ["ib-gateway", "ib-gateway:99999", ":4004", "ib-gateway:0", "ib-gateway:", "ib gateway:4004", "::1:4004", "fe80::1:4004"] {
            assert!(ext(bad).is_err(), "{bad:?} accepted");
        }
        // Stored without the field (older credential): managed.
        let old: IbkrTwsCredentials = serde_json::from_value(serde_json::json!({"username": "u", "password": "p"})).unwrap();
        assert_eq!(old.external().unwrap(), None);
    }

    #[test]
    fn managed_needs_login() {
        let why = "username and password are required unless a gateway address is set";
        assert_eq!(creds("", "p", "").check().unwrap_err(), why);
        assert_eq!(creds("u", "", "").check().unwrap_err(), why);
        assert_eq!(creds(" ", "p", "").check().unwrap_err(), why);
        assert!(creds("u", "p", "").check().is_ok());
    }

    #[test]
    fn external_needs_no_login() {
        assert!(creds("", "", "ib-gateway:4004").check().is_ok());
        assert!(creds("u", "p", "ib-gateway:4003").check().is_ok());
        assert!(creds("", "", "ib-gateway").check().is_err());
    }

    #[test]
    fn an_external_connection_is_remembered_until_it_goes_managed() {
        let code = "IBKR_TWS-EXTTEST";
        assert!(!is_external(code));
        set_external(code, Some("ib-gateway:4004".into()));
        assert!(is_external(code));
        assert_eq!(external_addr(code).as_deref(), Some("ib-gateway:4004"));
        set_external(code, None);
        assert!(!is_external(code));
    }

    #[test]
    fn gateway_errors_never_echo_the_address() {
        let e = creds("", "", "SECRETHOST:99999").check().unwrap_err();
        assert!(!e.contains("SECRETHOST"), "{e}");
    }
}
