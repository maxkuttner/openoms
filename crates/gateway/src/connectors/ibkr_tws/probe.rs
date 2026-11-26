//! The IBKR_TWS credential test: is IB Gateway running, is it logged in, and
//! can the OMS trade through it?
//!
//! The OMS's own session for the connection, if it is up (its accounts are
//! read and checked against the environment); else a short-lived probe
//! connection to the gateway's address. "Not running yet" and "not logged in yet" are warnings
//! — the address may well be saved before the gateway is up — while a gateway
//! logged into the wrong kind of account (paper vs live) fails and blocks the save.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{timeout_at, Instant};

use super::client::{TwsClient, TwsError};
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
    pub creds: &'a IbkrTwsCredentials,
    /// The OMS's connected client for this connection, when its session is up
    /// on the gateway these credentials name.
    pub live: Option<Arc<dyn TwsClient>>,
}

/// Tests `t`, spending at most `limit` on talking to a gateway. `connect`
/// opens a client to `host:port` as a client id (injected for tests).
pub async fn test<F, Fut>(t: Target<'_>, limit: Duration, connect: F) -> TestOutcome
where
    F: FnOnce(String, i32) -> Fut,
    Fut: Future<Output = Result<Arc<dyn TwsClient>, TwsError>>,
{
    let addr = match t.creds.address_for(t.env) {
        Ok((host, port)) => format!("{host}:{port}"),
        Err(why) => return TestOutcome::Failed(why),
    };
    let who = format!("IB Gateway at {addr}");

    if let Some(client) = t.live {
        return match accounts(client.as_ref(), Instant::now() + limit).await {
            Ok(accts) => match account_mismatch(&who, t.env, &accts) {
                Some(why) => TestOutcome::Failed(why),
                None => TestOutcome::PassedWith(format!("Connected to {who}; {}.", describe_accounts(&accts))),
            },
            Err(e) => TestOutcome::Warning(format!("Connected to {who}, but it reports no accounts ({e}).")),
        };
    }
    probe(&addr, t.env, t.creds.master_client_id, limit, connect).await
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

async fn probe<F, Fut>(addr: &str, env: &str, master_client_id: i32, limit: Duration, connect: F) -> TestOutcome
where
    F: FnOnce(String, i32) -> Fut,
    Fut: Future<Output = Result<Arc<dyn TwsClient>, TwsError>>,
{
    let deadline = Instant::now() + limit;
    let id = probe_client_id(master_client_id);
    let client = match timeout_at(deadline, connect(addr.to_string(), id)).await {
        Ok(Ok(c)) => c,
        Ok(Err(TwsError::ClientIdInUse(_) | TwsError::Api { code: 326, .. })) => {
            return TestOutcome::Warning(format!(
                "IB Gateway at {addr} answers, but client id {id} is in use by another API client. The credential test connects as client id {id}; close that client (or change its id) and test again."
            ))
        }
        Ok(Err(e)) => return unreachable(addr, &e.to_string()),
        Err(_) => return unreachable(addr, &format!("no answer within {}s", limit.as_secs())),
    };
    let read = accounts(client.as_ref(), deadline).await;
    client.shutdown();
    match read {
        Ok(accts) => match account_mismatch(&format!("IB Gateway at {addr}"), env, &accts) {
            Some(why) => TestOutcome::Failed(why),
            None => TestOutcome::PassedWith(format!(
                "IB Gateway at {addr} is up, logged into {}. The OMS connects within a minute after you save.",
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::client::FakeTws;
    use super::*;

    const LIMIT: Duration = Duration::from_millis(200);

    fn creds(gateway: &str) -> IbkrTwsCredentials {
        IbkrTwsCredentials { gateway: Some(gateway.into()), master_client_id: 0 }
    }

    fn fake(accounts: &[&str]) -> Arc<FakeTws> {
        let f = Arc::new(FakeTws::new());
        *f.accounts.lock().unwrap() = accounts.iter().map(|a| a.to_string()).collect();
        f
    }

    async fn run(env: &str, creds: &IbkrTwsCredentials, live: Option<Arc<dyn TwsClient>>, probe: Result<Arc<FakeTws>, TwsError>) -> TestOutcome {
        let t = Target { env, creds, live };
        test(t, LIMIT, |_, _| async move { probe.map(|c| c as Arc<dyn TwsClient>) }).await
    }

    fn never_probed() -> Result<Arc<FakeTws>, TwsError> {
        Err(TwsError::Other("the probe must not run while the session is up".into()))
    }

    #[tokio::test]
    async fn a_live_session_reports_its_account() {
        let ext = creds("ib-gateway:4004");
        let live: Arc<dyn TwsClient> = fake(&["DU1234567"]);
        assert_eq!(
            run("PAPER", &ext, Some(live), never_probed()).await,
            TestOutcome::PassedWith("Connected to IB Gateway at ib-gateway:4004; account DU1234567 (paper).".into())
        );
    }

    #[tokio::test]
    async fn a_live_session_on_the_wrong_kind_of_account_fails() {
        let ext = creds("ib-gateway:4004");
        let live: Arc<dyn TwsClient> = fake(&["U7654321"]);
        assert_eq!(
            run("PAPER", &ext, Some(live), never_probed()).await,
            TestOutcome::Failed("IB Gateway at ib-gateway:4004 is logged into a live account but this connection is PAPER".into())
        );
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_a_warning() {
        let ext = creds("ib-gateway:4004");
        let refused = Err(TwsError::Other("connection refused".into()));
        assert_eq!(
            run("PAPER", &ext, None, refused).await,
            TestOutcome::Warning(
                "IB Gateway at ib-gateway:4004 is not reachable (connection refused). Is it running and logged in (approve the IBKR Mobile prompt)? The OMS connects as soon as it answers.".into()
            )
        );
    }

    #[tokio::test]
    async fn a_hung_gateway_times_out_as_a_warning() {
        let ext = creds("ib-gateway:4004");
        let t = Target { env: "PAPER", creds: &ext, live: None };
        let out = test(t, LIMIT, |_, _| std::future::pending::<Result<Arc<dyn TwsClient>, TwsError>>()).await;
        assert!(matches!(&out, TestOutcome::Warning(m) if m.contains("is not reachable (no answer within")), "{out:?}");
    }

    #[tokio::test]
    async fn a_reachable_paper_gateway_passes_and_the_probe_disconnects() {
        let ext = creds("ib-gateway:4004");
        let probe = fake(&["DU1234567"]);
        let used = Arc::new(Mutex::new(None));
        let seen = used.clone();
        let p = probe.clone();
        let t = Target { env: "PAPER", creds: &ext, live: None };
        let out = test(t, LIMIT, move |addr, id| async move {
            *seen.lock().unwrap() = Some((addr, id));
            Ok(p as Arc<dyn TwsClient>)
        })
        .await;
        assert_eq!(
            out,
            TestOutcome::PassedWith("IB Gateway at ib-gateway:4004 is up, logged into account DU1234567 (paper). The OMS connects within a minute after you save.".into())
        );
        assert_eq!(*used.lock().unwrap(), Some(("ib-gateway:4004".to_string(), PROBE_CLIENT_ID)));
        assert!(!probe.connected(), "the probe client is shut down");
    }

    /// TWS error 326: another API client holds the probe's client id.
    #[tokio::test]
    async fn the_probes_client_id_in_use_says_so() {
        let ext = creds("ib-gateway:4004");
        for err in [TwsError::ClientIdInUse(PROBE_CLIENT_ID), TwsError::Api { code: 326, message: "in use".into() }] {
            assert_eq!(
                run("PAPER", &ext, None, Err(err)).await,
                TestOutcome::Warning(
                    "IB Gateway at ib-gateway:4004 answers, but client id 999 is in use by another API client. The credential test connects as client id 999; close that client (or change its id) and test again.".into()
                )
            );
        }
    }

    #[test]
    fn the_probe_never_takes_the_sessions_client_id() {
        assert_eq!(probe_client_id(0), PROBE_CLIENT_ID);
        assert_eq!(probe_client_id(PROBE_CLIENT_ID), PROBE_CLIENT_ID - 1);
    }

    #[tokio::test]
    async fn a_reachable_gateway_on_the_wrong_kind_of_account_fails() {
        let live_conn = creds("ib-gateway:4003");
        assert_eq!(
            run("LIVE", &live_conn, None, Ok(fake(&["DU1234567"]))).await,
            TestOutcome::Failed("IB Gateway at ib-gateway:4003 is logged into a paper account but this connection is LIVE".into())
        );
    }

    #[tokio::test]
    async fn a_reachable_gateway_with_no_accounts_is_not_logged_in() {
        let ext = creds("ib-gateway:4004");
        assert_eq!(
            run("PAPER", &ext, None, Ok(fake(&[]))).await,
            TestOutcome::Warning(
                "IB Gateway at ib-gateway:4004 answers but is not logged in yet (it reports no accounts). Approve the IBKR Mobile prompt; the OMS connects as soon as it is logged in.".into()
            )
        );
        let down = fake(&["DU1"]);
        down.set_connected(false);
        let out = run("PAPER", &ext, None, Ok(down)).await;
        assert!(matches!(&out, TestOutcome::Warning(m) if m.contains("answers but is not logged in yet (not connected to IB Gateway)")), "{out:?}");
    }

    #[tokio::test]
    async fn a_bad_address_or_port_for_the_environment_fails() {
        assert!(matches!(run("PAPER", &creds("ib-gateway"), None, never_probed()).await, TestOutcome::Failed(_)));
        assert!(matches!(run("PAPER", &creds("ib-gateway:4003"), None, never_probed()).await, TestOutcome::Failed(m) if m.contains("live port")));
        let no_address = IbkrTwsCredentials { gateway: None, master_client_id: 0 };
        assert!(matches!(run("PAPER", &no_address, None, never_probed()).await, TestOutcome::Failed(m) if m.starts_with("set the Gateway address")));
    }
}
