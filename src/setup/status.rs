//! `oms status`: what is running on this machine, and where.
//!
//! Reads the same settings `serve()` does (`oms.toml`, then the environment) to
//! learn where each piece *should* be, then checks whether it answers. The
//! dependencies are dialled directly, so they are reported even with the OMS
//! down; only the broker streams need the running OMS, which alone knows them.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use sqlx::Connection;

use gateway::stream_health::{StreamHealth, StreamKind, StreamState};

/// Long enough for a slow local service, short enough that a dead one does not
/// make `oms status` look hung.
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Up,
    Down,
    /// Not configured here, so there is nothing to check.
    Off,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            State::Up => "up",
            State::Down => "down",
            State::Off => "not configured",
        }
    }
}

pub struct Check {
    pub name: String,
    pub state: State,
    pub target: String,
    pub detail: String,
}

pub enum Streams {
    /// Why they could not be read, e.g. the OMS is not running.
    Unknown(String),
    List(Vec<StreamHealth>),
}

pub struct Report {
    pub config: Option<PathBuf>,
    pub oms: Check,
    pub uis: Vec<(String, String)>,
    pub deps: Vec<Check>,
    pub streams: Streams,
}

/// The address to dial for a bind address. A wildcard bind (`0.0.0.0`, `::`)
/// listens everywhere but cannot be dialled or clicked, so it becomes localhost.
pub fn reachable_addr(bind: &str) -> String {
    let (host, port) = match bind.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((host, rest)) => (host, rest.trim_start_matches(':')),
        None => bind.rsplit_once(':').unwrap_or((bind, "")),
    };
    if matches!(host, "0.0.0.0" | "::" | "") {
        format!("localhost:{port}")
    } else {
        bind.to_string()
    }
}

/// `KAFKA_BROKER` is librdkafka's comma-separated bootstrap list.
pub fn kafka_brokers(list: &str) -> Vec<String> {
    list.split(',').map(str::trim).filter(|b| !b.is_empty()).map(String::from).collect()
}

/// The admin UI is there when the bundle is built in; the trader UI only with
/// `[auth.oidc]` configured, the same gate `serve()` mounts `/trade/` behind.
pub fn ui_links(base: &str, bundled: bool, oidc: bool) -> Vec<(String, String)> {
    let mut links = Vec::new();
    if bundled {
        links.push(("admin UI".to_string(), format!("{base}/cockpit/")));
        if oidc {
            links.push(("trader UI".to_string(), format!("{base}/trade/")));
        }
    }
    links
}

pub fn exit_code(r: &Report) -> i32 {
    let streams_down = match &r.streams {
        Streams::List(list) => list.iter().any(|s| s.state == StreamState::Down),
        Streams::Unknown(_) => false,
    };
    let down = r.oms.state == State::Down || r.deps.iter().any(|c| c.state == State::Down) || streams_down;
    i32::from(down)
}

fn row(name: &str, state: &str, target: &str, detail: &str) -> String {
    format!("{name:<15} {state:<15} {target:<36} {detail}").trim_end().to_string()
}

pub fn render(r: &Report) -> String {
    let mut out = Vec::new();
    let config = match &r.config {
        Some(p) => p.display().to_string(),
        None => "none (defaults and environment only)".to_string(),
    };
    out.push(row("config", "", &config, ""));
    out.push(String::new());
    out.push(row(&r.oms.name, r.oms.state.word(), &r.oms.target, &r.oms.detail));
    for (name, url) in &r.uis {
        out.push(row(&format!("  {name}"), "", url, ""));
    }
    for c in &r.deps {
        out.push(row(&c.name, c.state.word(), &c.target, &c.detail));
    }
    match &r.streams {
        Streams::Unknown(why) => out.push(row("Broker streams", "unknown", why, "")),
        Streams::List(list) if list.is_empty() => out.push(row("Broker streams", "none running", "", "")),
        Streams::List(list) => {
            out.push("Broker streams".to_string());
            for s in list {
                let kind = match s.kind {
                    StreamKind::Feed => "feed",
                    StreamKind::Execution => "execution",
                };
                let state = match s.state {
                    StreamState::Live => "live",
                    StreamState::Connecting => "connecting",
                    StreamState::Down => "down",
                };
                let detail = if s.state == StreamState::Down { s.last_error.as_deref().unwrap_or("") } else { "" };
                out.push(row(&format!("  {}", s.broker_code), state, &format!("{} {kind}", s.environment), detail));
            }
        }
    }
    out.join("\n")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(TIMEOUT).build().expect("http client")
}

pub async fn check_oms(base: &str) -> Check {
    let (state, detail) = match client().get(format!("{base}/health")).send().await {
        Ok(resp) if resp.status().is_success() => (State::Up, String::new()),
        Ok(resp) => (State::Down, format!("/health answered {}", resp.status())),
        Err(e) if e.is_connect() => (State::Down, "not running".to_string()),
        Err(e) => (State::Down, e.to_string()),
    };
    Check { name: "OMS".into(), state, target: base.into(), detail }
}

pub async fn check_tcp(name: &str, addr: &str) -> Check {
    let (state, detail) = match tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => (State::Up, String::new()),
        Ok(Err(e)) => (State::Down, e.to_string()),
        Err(_) => (State::Down, "timed out".to_string()),
    };
    Check { name: name.into(), state, target: addr.into(), detail }
}

/// Logs in as the `oms` role, the way the server does, so a wrong password shows
/// up here and not only in the server log.
async fn check_postgres() -> Check {
    let cfg = super::database::config::resolve(Default::default());
    let target = format!("{}:{}/{}", cfg.host, cfg.port, cfg.database);
    let url = cfg.runtime_url(&super::database::config::resolve_role_password(None));
    let (state, detail) = match tokio::time::timeout(TIMEOUT, sqlx::PgConnection::connect(&url)).await {
        Ok(Ok(conn)) => {
            let _ = conn.close().await;
            (State::Up, String::new())
        }
        Ok(Err(e)) => (State::Down, e.to_string()),
        Err(_) => (State::Down, "timed out".to_string()),
    };
    Check { name: "Postgres".into(), state, target, detail }
}

async fn check_login(issuer: Option<String>) -> Check {
    let Some(issuer) = issuer else {
        return Check { name: "Login (OIDC)".into(), state: State::Off, target: String::new(), detail: String::new() };
    };
    let discovery = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let (state, detail) = match client().get(discovery).send().await {
        Ok(resp) if resp.status().is_success() => (State::Up, String::new()),
        Ok(resp) => (State::Down, format!("discovery answered {}", resp.status())),
        Err(e) if e.is_connect() => (State::Down, "not reachable".to_string()),
        Err(e) => (State::Down, e.to_string()),
    };
    Check { name: "Login (OIDC)".into(), state, target: issuer, detail }
}

/// Asked of the running OMS with the admin password, resolved exactly as the
/// server resolves it, so it works wherever this shell sees the same settings.
async fn streams(base: &str, file: Option<&crate::config::FileConfig>) -> Streams {
    let password = crate::resolve_admin_password(crate::admin_password_from_env(), file)
        .unwrap_or_default();
    let resp = match client().get(format!("{base}/admin/stream-health")).bearer_auth(password).send().await {
        Ok(resp) => resp,
        Err(e) => return Streams::Unknown(e.to_string()),
    };
    if !resp.status().is_success() {
        return Streams::Unknown(format!(
            "the OMS answered {} (stream health needs the legacy OMS_ADMIN_PASSWORD, which stops working once a named admin signs in)",
            resp.status()
        ));
    }
    match resp.json().await {
        Ok(list) => Streams::List(list),
        Err(e) => Streams::Unknown(e.to_string()),
    }
}

/// Prints the report and returns the exit code: 0 when everything configured
/// is up, 1 when anything is down.
pub async fn run() -> i32 {
    let file = crate::config::load();
    let path = crate::config::path();
    let config = path.exists().then(|| crate::config::absolute(path));

    let bind = crate::resolve_bind_addr(env::var("OMS_BIND_ADDR").ok(), file);
    let base = format!("http://{}", reachable_addr(&bind));
    let issuer = file.and_then(|f| f.auth.oidc.issuer.clone()).filter(|i| !i.is_empty());
    let kafka = env::var("KAFKA_BROKER").ok().map(|v| kafka_brokers(&v)).unwrap_or_default();

    let (oms, postgres, login) = tokio::join!(check_oms(&base), check_postgres(), check_login(issuer.clone()));
    let mut deps = vec![postgres, login];
    if kafka.is_empty() {
        deps.push(Check { name: "Kafka".into(), state: State::Off, target: String::new(), detail: String::new() });
    }
    for broker in &kafka {
        deps.push(check_tcp("Kafka", broker).await);
    }

    let (uis, streams) = if oms.state == State::Up {
        (ui_links(&base, crate::cockpit::is_bundled(), issuer.is_some()), streams(&base, file).await)
    } else {
        (Vec::new(), Streams::Unknown("OMS not running".into()))
    };

    let report = Report { config, oms, uis, deps, streams };
    println!("{}", render(&report));
    exit_code(&report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway::stream_health::{StreamHealth, StreamKind, StreamState, StreamTransport};

    fn check(name: &str, state: State, target: &str, detail: &str) -> Check {
        Check { name: name.into(), state, target: target.into(), detail: detail.into() }
    }

    fn stream(code: &str, state: StreamState, error: Option<&str>) -> StreamHealth {
        StreamHealth {
            broker_code: code.into(),
            environment: "paper".into(),
            kind: StreamKind::Execution,
            transport: StreamTransport::WebSocket,
            state,
            connected_since: None,
            last_event_at: None,
            last_error: error.map(Into::into),
        }
    }

    fn report(oms: State, kafka: State, streams: Streams) -> Report {
        Report {
            config: Some("/home/me/.config/oms/oms.toml".into()),
            oms: check("OMS", oms, "http://localhost:3001", ""),
            uis: ui_links("http://localhost:3001", true, true),
            deps: vec![
                check("Postgres", State::Up, "localhost:5432/ods", ""),
                check("Kafka", kafka, "localhost:9092", "connection refused"),
            ],
            streams,
        }
    }

    #[test]
    fn a_wildcard_bind_is_probed_and_shown_as_localhost() {
        assert_eq!(reachable_addr("0.0.0.0:3001"), "localhost:3001");
        assert_eq!(reachable_addr("[::]:3001"), "localhost:3001");
        assert_eq!(reachable_addr("127.0.0.1:8080"), "127.0.0.1:8080");
        assert_eq!(reachable_addr("oms.lan:3001"), "oms.lan:3001");
    }

    #[test]
    fn kafka_brokers_are_split_on_commas() {
        assert_eq!(kafka_brokers("a:9092, b:9093,"), vec!["a:9092", "b:9093"]);
    }

    #[test]
    fn the_trader_ui_is_listed_only_with_login_configured() {
        let with = ui_links("http://localhost:3001", true, true);
        assert_eq!(
            with,
            vec![
                ("admin UI".to_string(), "http://localhost:3001/cockpit/".to_string()),
                ("trader UI".to_string(), "http://localhost:3001/trade/".to_string()),
            ]
        );
        let without = ui_links("http://localhost:3001", true, false);
        assert_eq!(without.len(), 1);
        assert!(ui_links("http://localhost:3001", false, false).is_empty());
    }

    #[test]
    fn everything_up_exits_zero_and_anything_down_exits_one() {
        let ok = report(State::Up, State::Off, Streams::List(vec![stream("alpaca-paper", StreamState::Live, None)]));
        assert_eq!(exit_code(&ok), 0);
        assert_eq!(exit_code(&report(State::Up, State::Down, Streams::List(vec![]))), 1);
        assert_eq!(exit_code(&report(State::Down, State::Up, Streams::Unknown("OMS not running".into()))), 1);
        let dropped = Streams::List(vec![stream("ibkr-paper", StreamState::Down, Some("reset"))]);
        assert_eq!(exit_code(&report(State::Up, State::Up, dropped)), 1);
    }

    #[test]
    fn the_report_names_each_piece_its_state_and_where_it_is() {
        let r = report(
            State::Up,
            State::Down,
            Streams::List(vec![
                stream("alpaca-paper", StreamState::Live, None),
                stream("ibkr-paper", StreamState::Down, Some("connection reset")),
            ]),
        );
        let out = render(&r);
        let line = |needle: &str| out.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no {needle} in:\n{out}")).to_string();
        assert!(line("config").contains("/home/me/.config/oms/oms.toml"));
        assert!(line("OMS").contains("up") && line("OMS").contains("http://localhost:3001"));
        assert!(line("admin UI").contains("http://localhost:3001/cockpit/"));
        assert!(line("Kafka").contains("down") && line("Kafka").contains("connection refused"));
        assert!(line("alpaca-paper").contains("live"));
        assert!(line("ibkr-paper").contains("down") && line("ibkr-paper").contains("connection reset"));
    }

    #[test]
    fn with_the_oms_down_the_streams_say_why_they_are_unknown() {
        let out = render(&report(State::Down, State::Up, Streams::Unknown("OMS not running".into())));
        assert!(out.contains("unknown") && out.contains("OMS not running"), "{out}");
    }

    #[test]
    fn no_config_file_says_so() {
        let mut r = report(State::Up, State::Up, Streams::List(vec![]));
        r.config = None;
        assert!(render(&r).lines().next().unwrap().contains("none"));
    }

    #[test]
    fn stream_health_reads_back_what_the_server_sends() {
        let sent = vec![stream("alpaca-paper", StreamState::Down, Some("reset"))];
        let back: Vec<StreamHealth> = serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        assert_eq!(back[0].broker_code, "alpaca-paper");
        assert!(back[0].state == StreamState::Down);
    }

    #[tokio::test]
    async fn a_running_oms_is_up_and_a_closed_port_is_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/health", axum::routing::get(|| async { "OK" }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(check_oms(&format!("http://{addr}")).await.state == State::Up);

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let down = check_oms(&format!("http://{closed}")).await;
        assert!(down.state == State::Down);
        assert!(down.detail.contains("not running"), "{}", down.detail);
    }

    #[tokio::test]
    async fn a_tcp_dependency_is_down_when_nothing_listens() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up = check_tcp("Kafka", &open.local_addr().unwrap().to_string()).await;
        assert!(up.state == State::Up);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(check_tcp("Kafka", &closed.to_string()).await.state == State::Down);
    }
}
