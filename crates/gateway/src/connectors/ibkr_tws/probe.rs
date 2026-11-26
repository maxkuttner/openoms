//! The IBKR_TWS credential test: is IB Gateway running, is it logged in, and
//! can the OMS trade through it?
//!
//! In order: the OMS's own session for the connection, if it is up (its
//! accounts are read and checked against the environment); else, for an
//! external gateway, a short-lived probe connection to its address; else, for
//! the OMS-run gateway, what Docker says about its container. The test never
//! starts a container. "Not running yet" and "not logged in yet" are warnings
//! — the address may well be saved before the gateway is up — while a gateway
//! logged into the wrong kind of account (paper vs live) fails and blocks the save.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{timeout_at, Instant};

use super::client::{TwsClient, TwsError};
use super::gateway::{classify_logs, container_name, GatewayDocker, GatewayState};
use super::stream::{account_mismatch, paper_accounts};
use super::IbkrTwsCredentials;
use crate::verify::TestOutcome;

/// The TWS API client id the probe connects as, so it never takes the id the
/// OMS's session uses (`master_client_id`, 0 by default) or kicks it off; when
/// that is 999 itself, the probe uses 998. The probe disconnects right after
/// reading the accounts.
pub const PROBE_CLIENT_ID: i32 = 999;

fn probe_client_id(master_client_id: i32) -> i32 {
    if master_client_id == PROBE_CLIENT_ID {
        PROBE_CLIENT_ID - 1
    } else {
        PROBE_CLIENT_ID
    }
}

/// What is tested.
pub struct Target<'a> {
    /// The connection's environment: `PAPER` or `LIVE`.
    pub env: &'a str,
    /// The connection code (`IBKR_TWS-PAPER`): names the OMS-run container.
    pub code: &'a str,
    pub creds: &'a IbkrTwsCredentials,
    /// The OMS's connected client for this connection, when its session is up
    /// on the gateway these credentials name.
    pub live: Option<Arc<dyn TwsClient>>,
    pub docker: Arc<dyn GatewayDocker>,
}

/// Tests `t`, spending at most `limit` on talking to a gateway. `connect`
/// opens a client to `host:port` as a client id (injected for tests).
pub async fn test<F, Fut>(t: Target<'_>, limit: Duration, connect: F) -> TestOutcome
where
    F: FnOnce(String, i32) -> Fut,
    Fut: Future<Output = Result<Arc<dyn TwsClient>, TwsError>>,
{
    let external = match t.creds.external() {
        Ok(e) => e.map(|(host, port)| format!("{host}:{port}")),
        Err(why) => return TestOutcome::Failed(why),
    };
    if let Err(why) = t.creds.check_environment(t.env) {
        return TestOutcome::Failed(why);
    }
    let who = match &external {
        Some(addr) => format!("IB Gateway at {addr}"),
        None => "The OMS-run IB Gateway".to_string(),
    };

    if let Some(client) = t.live {
        let place = match &external {
            Some(addr) => format!("IB Gateway at {addr}"),
            None => "the OMS-run IB Gateway".to_string(),
        };
        return match accounts(client.as_ref(), Instant::now() + limit).await {
            Ok(accts) => match account_mismatch(&who, t.env, &accts) {
                Some(why) => TestOutcome::Failed(why),
                None => TestOutcome::PassedWith(format!("Connected to {place}; {}.", describe_accounts(&accts))),
            },
            Err(e) => TestOutcome::Warning(format!("Connected to {place}, but it reports no accounts ({e}).")),
        };
    }

    match external {
        Some(addr) => probe_external(&addr, t.env, t.creds.master_client_id, limit, connect).await,
        None => managed(t.code, t.docker).await,
    }
}

/// "account DU1234567 (paper)", or "accounts U1, U2 (live)".
fn describe_accounts(accts: &[String]) -> String {
    let kind = if paper_accounts(accts) { "paper" } else { "live" };
    let noun = if accts.len() == 1 { "account" } else { "accounts" };
    format!("{noun} {} ({kind})", accts.join(", "))
}

/// The gateway's accounts by `deadline`; none is an error (not logged in).
async fn accounts(client: &dyn TwsClient, deadline: Instant) -> Result<Vec<String>, String> {
    match timeout_at(deadline, client.managed_accounts()).await {
        Ok(Ok(a)) if a.is_empty() => Err("it reports no accounts".into()),
        Ok(Ok(a)) => Ok(a),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("no answer in time".into()),
    }
}

async fn probe_external<F, Fut>(addr: &str, env: &str, master_client_id: i32, limit: Duration, connect: F) -> TestOutcome
where
    F: FnOnce(String, i32) -> Fut,
    Fut: Future<Output = Result<Arc<dyn TwsClient>, TwsError>>,
{
    let deadline = Instant::now() + limit;
    let client = match timeout_at(deadline, connect(addr.to_string(), probe_client_id(master_client_id))).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return unreachable(addr, &e.to_string()),
        Err(_) => return unreachable(addr, &format!("no answer within {}s", limit.as_secs())),
    };
    let read = accounts(client.as_ref(), deadline).await;
    client.shutdown();
    match read {
        Ok(accts) => match account_mismatch(&format!("IB Gateway at {addr}"), env, &accts) {
            Some(why) => TestOutcome::Failed(why),
            None => TestOutcome::PassedWith(format!(
                "IB Gateway at {addr} is up, logged into {}. The OMS will connect within a minute.",
                describe_accounts(&accts)
            )),
        },
        Err(e) => TestOutcome::Warning(format!(
            "IB Gateway at {addr} answers but is not logged in yet ({e}). Approve the IBKR Mobile prompt; the OMS connects as soon as it is logged in."
        )),
    }
}

fn unreachable(addr: &str, why: &str) -> TestOutcome {
    TestOutcome::Warning(format!(
        "IB Gateway at {addr} is not reachable ({why}). Is it running and logged in (approve the IBKR Mobile prompt)? The OMS connects as soon as it answers."
    ))
}

/// The OMS-run gateway with no session up: what Docker says about its container.
async fn managed(code: &str, docker: Arc<dyn GatewayDocker>) -> TestOutcome {
    let name = container_name(code);
    let looked = tokio::task::spawn_blocking(move || {
        if !docker.available() {
            return None;
        }
        let info = docker.inspect(&name);
        let tail = if info.is_some() { docker.logs_tail(&name, 200) } else { String::new() };
        Some((info, tail))
    })
    .await
    .unwrap_or(None);
    let Some((info, tail)) = looked else {
        return TestOutcome::Warning(
            "Docker is not available to the OMS, so it cannot start IB Gateway. Run IB Gateway yourself and set Gateway address.".into(),
        );
    };
    let Some(info) = info else {
        return TestOutcome::Warning(
            "IB Gateway is not running: the OMS has not started it yet. It starts it in Docker once this connection is saved and enabled.".into(),
        );
    };
    TestOutcome::Warning(match classify_logs(&tail) {
        Some(GatewayState::LoginFailed(line)) => {
            format!("IB Gateway's last login failed ({line}). Check the username and password; saving new ones restarts it.")
        }
        Some(GatewayState::AwaitingTwoFactor) => {
            "IB Gateway is running and waiting for you to approve the login in IBKR Mobile.".into()
        }
        Some(GatewayState::ExistingSession) => {
            "IB Gateway is running and taking over a session this IBKR user has open elsewhere; the OMS connects once it is logged in.".into()
        }
        _ if info.running => "IB Gateway is running but not logged in yet; the OMS connects as soon as it answers.".into(),
        _ => "IB Gateway's container is stopped; the OMS starts it again on its next connect attempt.".into(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::client::FakeTws;
    use super::super::gateway::{ContainerInfo, RunSpec};
    use super::*;

    const LIMIT: Duration = Duration::from_millis(200);

    fn creds(gateway: Option<&str>, username: &str) -> IbkrTwsCredentials {
        IbkrTwsCredentials {
            username: username.into(),
            password: if username.is_empty() { String::new() } else { "pw".into() },
            gateway: gateway.map(str::to_string),
            master_client_id: 0,
        }
    }

    fn fake(accounts: &[&str]) -> Arc<FakeTws> {
        let f = Arc::new(FakeTws::new());
        *f.accounts.lock().unwrap() = accounts.iter().map(|a| a.to_string()).collect();
        f
    }

    /// Docker as the test scripts it; `None` = Docker not available.
    struct Docker(Option<(Option<ContainerInfo>, &'static str)>);

    impl GatewayDocker for Docker {
        fn inspect(&self, _: &str) -> Option<ContainerInfo> {
            self.0.as_ref().unwrap().0.clone()
        }
        fn run(&self, _: &RunSpec) -> Result<(), String> {
            panic!("the test must never start a container")
        }
        fn stop_rm(&self, _: &str) -> Result<(), String> {
            panic!("the test must never remove a container")
        }
        fn logs_tail(&self, _: &str, _: usize) -> String {
            self.0.as_ref().unwrap().1.into()
        }
        fn available(&self) -> bool {
            self.0.is_some()
        }
    }

    fn no_docker_calls() -> Arc<dyn GatewayDocker> {
        struct Panic;
        impl GatewayDocker for Panic {
            fn inspect(&self, _: &str) -> Option<ContainerInfo> {
                panic!("docker inspect")
            }
            fn run(&self, _: &RunSpec) -> Result<(), String> {
                panic!("docker run")
            }
            fn stop_rm(&self, _: &str) -> Result<(), String> {
                panic!("docker rm")
            }
            fn logs_tail(&self, _: &str, _: usize) -> String {
                panic!("docker logs")
            }
            fn available(&self) -> bool {
                panic!("docker info")
            }
        }
        Arc::new(Panic)
    }

    async fn run(env: &str, creds: &IbkrTwsCredentials, live: Option<Arc<dyn TwsClient>>, docker: Arc<dyn GatewayDocker>, probe: Result<Arc<FakeTws>, TwsError>) -> TestOutcome {
        let t = Target { env, code: "IBKR_TWS-PAPER", creds, live, docker };
        test(t, LIMIT, |_, _| async move { probe.map(|c| c as Arc<dyn TwsClient>) }).await
    }

    fn never_probed() -> Result<Arc<FakeTws>, TwsError> {
        Err(TwsError::Other("the probe must not run while the session is up".into()))
    }

    #[tokio::test]
    async fn a_live_session_reports_its_account() {
        let ext = creds(Some("ib-gateway:4004"), "");
        let live: Arc<dyn TwsClient> = fake(&["DU1234567"]);
        assert_eq!(
            run("PAPER", &ext, Some(live), no_docker_calls(), never_probed()).await,
            TestOutcome::PassedWith("Connected to IB Gateway at ib-gateway:4004; account DU1234567 (paper).".into())
        );
        let managed = creds(None, "trader1");
        let live: Arc<dyn TwsClient> = fake(&["DU1234567"]);
        assert_eq!(
            run("PAPER", &managed, Some(live), no_docker_calls(), never_probed()).await,
            TestOutcome::PassedWith("Connected to the OMS-run IB Gateway; account DU1234567 (paper).".into())
        );
    }

    #[tokio::test]
    async fn a_live_session_on_the_wrong_kind_of_account_fails() {
        let ext = creds(Some("ib-gateway:4004"), "");
        let live: Arc<dyn TwsClient> = fake(&["U7654321"]);
        assert_eq!(
            run("PAPER", &ext, Some(live), no_docker_calls(), never_probed()).await,
            TestOutcome::Failed("IB Gateway at ib-gateway:4004 is logged into a live account but this connection is PAPER".into())
        );
    }

    #[tokio::test]
    async fn an_unreachable_external_gateway_is_a_warning() {
        let ext = creds(Some("ib-gateway:4004"), "");
        let refused = Err(TwsError::Other("connection refused".into()));
        assert_eq!(
            run("PAPER", &ext, None, no_docker_calls(), refused).await,
            TestOutcome::Warning(
                "IB Gateway at ib-gateway:4004 is not reachable (connection refused). Is it running and logged in (approve the IBKR Mobile prompt)? The OMS connects as soon as it answers.".into()
            )
        );
    }

    #[tokio::test]
    async fn a_hung_external_gateway_times_out_as_a_warning() {
        let ext = creds(Some("ib-gateway:4004"), "");
        let t = Target { env: "PAPER", code: "IBKR_TWS-PAPER", creds: &ext, live: None, docker: no_docker_calls() };
        let out = test(t, LIMIT, |_, _| std::future::pending::<Result<Arc<dyn TwsClient>, TwsError>>()).await;
        assert!(matches!(&out, TestOutcome::Warning(m) if m.contains("is not reachable (no answer within")), "{out:?}");
    }

    #[tokio::test]
    async fn a_reachable_paper_gateway_passes_and_the_probe_disconnects() {
        let ext = creds(Some("ib-gateway:4004"), "");
        let probe = fake(&["DU1234567"]);
        let used = Arc::new(Mutex::new(None));
        let seen = used.clone();
        let p = probe.clone();
        let t = Target { env: "PAPER", code: "IBKR_TWS-PAPER", creds: &ext, live: None, docker: no_docker_calls() };
        let out = test(t, LIMIT, move |addr, id| async move {
            *seen.lock().unwrap() = Some((addr, id));
            Ok(p as Arc<dyn TwsClient>)
        })
        .await;
        assert_eq!(
            out,
            TestOutcome::PassedWith("IB Gateway at ib-gateway:4004 is up, logged into account DU1234567 (paper). The OMS will connect within a minute.".into())
        );
        assert_eq!(*used.lock().unwrap(), Some(("ib-gateway:4004".to_string(), PROBE_CLIENT_ID)));
        assert!(!probe.connected(), "the probe client is shut down");
    }

    #[test]
    fn the_probe_never_takes_the_sessions_client_id() {
        assert_eq!(probe_client_id(0), PROBE_CLIENT_ID);
        assert_eq!(probe_client_id(PROBE_CLIENT_ID), PROBE_CLIENT_ID - 1);
    }

    #[tokio::test]
    async fn a_reachable_gateway_on_the_wrong_kind_of_account_fails() {
        let live_conn = creds(Some("ib-gateway:4003"), "");
        assert_eq!(
            run("LIVE", &live_conn, None, no_docker_calls(), Ok(fake(&["DU1234567"]))).await,
            TestOutcome::Failed("IB Gateway at ib-gateway:4003 is logged into a paper account but this connection is LIVE".into())
        );
    }

    #[tokio::test]
    async fn a_reachable_gateway_with_no_accounts_is_not_logged_in() {
        let ext = creds(Some("ib-gateway:4004"), "");
        assert_eq!(
            run("PAPER", &ext, None, no_docker_calls(), Ok(fake(&[]))).await,
            TestOutcome::Warning(
                "IB Gateway at ib-gateway:4004 answers but is not logged in yet (it reports no accounts). Approve the IBKR Mobile prompt; the OMS connects as soon as it is logged in.".into()
            )
        );
        let down = fake(&["DU1"]);
        down.set_connected(false);
        let out = run("PAPER", &ext, None, no_docker_calls(), Ok(down)).await;
        assert!(matches!(&out, TestOutcome::Warning(m) if m.contains("answers but is not logged in yet (not connected to IB Gateway)")), "{out:?}");
    }

    #[tokio::test]
    async fn managed_without_docker_says_to_run_the_gateway_yourself() {
        let managed = creds(None, "trader1");
        assert_eq!(
            run("PAPER", &managed, None, Arc::new(Docker(None)), never_probed()).await,
            TestOutcome::Warning(
                "Docker is not available to the OMS, so it cannot start IB Gateway. Run IB Gateway yourself and set Gateway address.".into()
            )
        );
    }

    #[tokio::test]
    async fn managed_reports_the_container_state() {
        let managed = creds(None, "trader1");
        let info = |running| ContainerInfo { running, api_port: Some(4100), container_port: Some(4004), loopback_only: true, creds_tag: None };
        let state = |d: Docker| {
            let managed = managed.clone();
            async move {
                match run("PAPER", &managed, None, Arc::new(d), never_probed()).await {
                    TestOutcome::Warning(m) => m,
                    other => panic!("expected a warning, got {other:?}"),
                }
            }
        };
        assert!(state(Docker(Some((None, "")))).await.contains("has not started it yet"));
        assert!(state(Docker(Some((Some(info(true)), "starting IBC")))).await.contains("running but not logged in yet"));
        assert!(state(Docker(Some((Some(info(true)), "Second Factor Authentication initiated")))).await.contains("approve the login in IBKR Mobile"));
        assert!(state(Docker(Some((Some(info(true)), "Existing session detected")))).await.contains("taking over a session"));
        assert!(state(Docker(Some((Some(info(false)), "Login failed")))).await.starts_with("IB Gateway's last login failed"));
        assert!(state(Docker(Some((Some(info(false)), "")))).await.contains("container is stopped"));
    }

    #[tokio::test]
    async fn a_bad_address_or_port_for_the_environment_fails() {
        assert!(matches!(run("PAPER", &creds(Some("ib-gateway"), ""), None, no_docker_calls(), never_probed()).await, TestOutcome::Failed(_)));
        assert!(matches!(run("PAPER", &creds(Some("ib-gateway:4003"), ""), None, no_docker_calls(), never_probed()).await, TestOutcome::Failed(m) if m.contains("live port")));
    }
}
