//! Turning the credential store into live adapters — at boot and again on demand.
//!
//! Boot and reload share this module so a credential registered at startup and
//! one registered at runtime cannot diverge. The split that matters is which
//! connections can be rebuilt in a running process at all: a REST adapter is an
//! HTTP client and swapping it is free, while a FIX session owns an OS thread
//! that parks forever with no stop path (`fix::start_session`), so replacing one
//! would mean a second session logging in against the first.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::event_bus::EventBus;
use crate::adapters::{BrokerAdapter, BrokerRegistry};
use crate::connectors::{self, ExecStream};
use crate::app_state::{AppState, StreamRegistry};
use crate::credentials::{BrokerCredentials, Connection, CredentialState};
use crate::opra_stream::DatabentoOpraFeed;
use crate::quote_feed::QuoteFeedSession;
use gateway::stream_health::{StreamHealthRegistry, StreamKind};
use gateway::stream_supervisor;

/// What happened to one connection during a registration pass.
///
/// `Serialize` because this is an HTTP response body in Task 5. Serde's default
/// external tagging gives `"Registered"` / `"RestartRequired"` for the unit
/// variants and `{"Failed": "..."}` for the one carrying a reason, which is what
/// the cockpit will match on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum ConnectionOutcome {
    Registered,
    /// The row exists but holds no credential — "needs setup".
    Unconfigured,
    /// `status <> 'ACTIVE'`; deliberately not registered.
    Disabled,
    /// Stored but unusable, or the adapter would not build. Carries the reason.
    Failed(String),
    /// Configured and valid, but changing it needs a process restart. Only FIX.
    RestartRequired,
}

/// The outcome of every connection in one registration pass — the shape Task 5
/// hands back as the reload endpoint's JSON body.
#[derive(Debug, serde::Serialize)]
pub struct ReloadReport {
    pub connections: Vec<(String, ConnectionOutcome)>,
}

/// Decide what to do with one connection.
///
/// `is_boot` is the whole reason this is a function rather than a match inline:
/// at boot a FIX connection is registered because nothing is running yet, while
/// on reload the same connection must report `RestartRequired` instead.
pub fn classify(conn: &Connection<BrokerCredentials>, is_boot: bool) -> ConnectionOutcome {
    if conn.status != "ACTIVE" {
        return ConnectionOutcome::Disabled;
    }
    match &conn.credentials {
        CredentialState::Unconfigured => ConnectionOutcome::Unconfigured,
        CredentialState::Error(e) => ConnectionOutcome::Failed(e.clone()),
        // A session that owns a running logon (FIX) cannot be swapped under
        // itself; at boot nothing is running yet, so it simply starts. A kind no
        // connector owns is left to registration, which reports why.
        CredentialState::Configured(c) => match connectors::by_credential_kind(&c.kind) {
            Some(connector) if connector.manifest().session_bound && !is_boot => ConnectionOutcome::RestartRequired,
            _ => ConnectionOutcome::Registered,
        },
    }
}

/// What the FIX starters and freshly-built adapters need beyond the credential
/// itself. Built once in `serve()` and handed to every call of `build_registry`
/// — boot today, a runtime reload endpoint in a later task — so both draw on the
/// exact same stream-health registry and order engine rather than each
/// constructing (or re-deriving) their own.
pub struct RegistrationDeps {
    pub stream_health: StreamHealthRegistry,
    pub engine: gateway::contract::EngineHandle,
}

/// Everything `build_registry` produces: the registry, the per-connection report,
/// and the execution streams the caller must (re)start. The streams are kept off
/// `ReloadReport`, which is serialised straight into an HTTP response.
///
/// `exec_streams` holds only connections built fresh this pass. A session-bound
/// connection carried forward on reload (`RestartRequired`) is not in it, so a
/// handler that restarts every entry restarts exactly what changed — never a
/// second stream beside one still running.
pub struct RegistrationOutput {
    pub registry: BrokerRegistry,
    pub report: ReloadReport,
    /// The execution stream of every connection built fresh this pass, for the
    /// caller to (re)start — at boot and on reload alike. A connection carried
    /// forward (restart required, or an unreadable credential) is not in it and
    /// keeps the stream it already has.
    pub exec_streams: Vec<ExecStreamStart>,
}

/// One connection's execution stream, ready to (re)start.
pub struct ExecStreamStart {
    pub code: &'static str,
    pub env: &'static str,
    /// The adapter the stream books against — the one just registered.
    pub adapter: Arc<dyn BrokerAdapter>,
    pub start: ExecStream,
}

/// Turn every broker connection into an entry in a fresh `BrokerRegistry`.
///
/// `is_boot` drives `classify`'s FIX decision (see there). When a connection
/// classifies as `RestartRequired`, its existing adapter is carried forward
/// from `current` rather than dropped or rebuilt — starting a second FIX
/// session against the same venue would collide on logon and sequence numbers
/// with the one already running, and simply not registering it would silently
/// disarm a working session, which is worse than declining to apply a change.
/// A row whose stored credential fails to decrypt (`CredentialState::Error`)
/// carries its existing adapter forward the same way, for the same reason —
/// see the `Error` arm below — even though nothing about *that* case requires
/// avoiding a second session; one bad or re-sealed row simply should not be
/// able to disarm a connection that was working a moment ago. At boot
/// `current` is an empty registry, so there is nothing to carry and every
/// connection is built fresh — this is what makes boot's call to this
/// function produce exactly the registry `serve()` used to build inline.
pub async fn build_registry(
    connections: &[Connection<BrokerCredentials>],
    is_boot: bool,
    current: &BrokerRegistry,
    deps: &RegistrationDeps,
) -> RegistrationOutput {
    let mut registry = BrokerRegistry::new();
    let mut exec_streams = Vec::new();
    let mut report = ReloadReport { connections: Vec::new() };

    for conn in connections {
        let mut outcome = classify(conn, is_boot);

        if matches!(outcome, ConnectionOutcome::Disabled) {
            info!(code = %conn.code, kind = %conn.kind, "broker connection disabled, skipping");
            report.connections.push((conn.code.clone(), outcome));
            continue;
        }

        // broker_connection.environment is DB-checked to ('PAPER'|'LIVE'); anything
        // else would be a schema mismatch, not operator input to gently degrade.
        let env_name: &'static str = match conn.environment.as_deref() {
            Some("PAPER") => "PAPER",
            Some("LIVE") => "LIVE",
            other => {
                error!(code = %conn.code, kind = %conn.kind, environment = ?other, "broker connection has an unrecognised environment, skipping");
                report.connections.push((conn.code.clone(), ConnectionOutcome::Failed("unrecognised environment".into())));
                continue;
            }
        };

        match &conn.credentials {
            CredentialState::Unconfigured => {
                info!(code = %conn.code, kind = %conn.kind, "no credentials stored, adapter not registered");
            }
            CredentialState::Error(e) => {
                error!(code = %conn.code, kind = %conn.kind, "credentials unusable: {e}");
                // Carry the existing adapter forward, the same deliberate trade
                // `RestartRequired` makes just below: the in-memory adapter (if
                // any) was built from a credential that decrypted fine the last
                // time this ran, and a *stored* row going unreadable — a
                // `rotate-key` run racing this reload, a hand-edited row, bit
                // rot — is not evidence the broker itself stopped working. One
                // bad row must not be able to disarm a connection that was
                // routing orders a moment ago; that would make `Error` strictly
                // worse than `RestartRequired`'s own carry-forward for no
                // reason. The report still says `Failed` (set by `classify`),
                // so the operator is told even though the previous adapter
                // keeps serving orders. `conn.kind`/`env_name` (plaintext DB
                // columns), not the credential, identify what to look for in
                // `current` — the credential itself is exactly what failed to
                // decode, so it cannot be consulted here.
                if let Some(adapter) = current.get(&conn.kind, env_name) {
                    registry.register(&conn.kind, env_name, adapter);
                }
            }
            CredentialState::Configured(creds) => {
                // Registration follows the stored credential's kind, not
                // `conn.kind` — two independent sources of truth for "what
                // broker is this", normally in lockstep but not enforced (a
                // credential hand-imported into the wrong row). The mismatch is
                // a misconfiguration worth surfacing, not something to route by.
                match connectors::by_credential_kind(&creds.kind) {
                    None => {
                        error!(code = %conn.code, credential_kind = %creds.kind, "no connector for this credential kind, adapter not registered");
                        outcome = ConnectionOutcome::Failed(format!("no connector for credential kind {}", creds.kind));
                    }
                    Some(connector) => {
                        let m = connector.manifest();
                        if conn.kind != m.code {
                            warn!(
                                code = %conn.code, kind = %conn.kind, credential_kind = m.code,
                                "broker_connection's broker_code does not match its stored credential's kind"
                            );
                        }
                        if m.live_only && env_name != "LIVE" {
                            // No paper venue: its adapter always talks to the live
                            // venue, so a non-LIVE label would hide real-money orders.
                            let name = (m.venue)().name;
                            error!(code = %conn.code, environment = env_name, "{name} connection is not LIVE, adapter not registered");
                            outcome = ConnectionOutcome::Failed(format!("{name} is LIVE only; this connection's environment is not LIVE"));
                        } else if matches!(outcome, ConnectionOutcome::RestartRequired) {
                            // Carry the running session forward rather than starting a
                            // second one against the same venue (see fn doc comment).
                            match current.get(m.code, env_name) {
                                Some(adapter) => registry.register(m.code, env_name, adapter),
                                // Nothing was actually running to carry forward — a
                                // materially different state from "restart to pick up
                                // your change": the operator needs to know there is no
                                // fallback adapter serving this connection right now.
                                None => {
                                    outcome = ConnectionOutcome::Failed(
                                        "connection needs a restart to apply this change and has no running adapter to fall back on".into(),
                                    );
                                }
                            }
                        } else {
                            match connector.connect(env_name, creds, deps) {
                                Ok(connected) => {
                                    registry.register(m.code, env_name, connected.adapter.clone());
                                    if let Some(start) = connected.exec_stream {
                                        exec_streams.push(ExecStreamStart { code: m.code, env: env_name, adapter: connected.adapter, start });
                                    }
                                    info!(code = %conn.code, credentials_updated_at = ?conn.credentials_updated_at, "registered {}/{env_name} adapter", m.code);
                                }
                                Err(reason) => {
                                    error!(code = %conn.code, "{reason}");
                                    outcome = ConnectionOutcome::Failed(reason);
                                }
                            }
                        }
                    }
                }
            }
        }
        report.connections.push((conn.code.clone(), outcome));
    }

    RegistrationOutput { registry, report, exec_streams }
}

/// (Re)start the Databento OPRA feed session under (possibly new) credentials,
/// publishing the fresh handle into `streams` under the code ("databento-opra")
/// this feed always runs under. The fresh session subscribes to the same bus's
/// order lane every other feed listens on.
///
/// Only Databento is credential-driven among the market-data feeds — Binance
/// and Bybit are public and are never restarted here or anywhere else (see
/// `StreamRegistry`'s doc comment in `app_state.rs`). That is also why this
/// function, unlike `build_registry`, does not loop over a slice of
/// connections: there is exactly one credentialed feed to restart.
///
/// Calling this when nothing is registered yet (boot) is safe: `abort_and_remove`
/// on an empty registry is a no-op, so boot and a later credential-driven
/// restart are the same call.
pub async fn restart_databento_feed(
    api_key: String,
    pool: PgPool,
    stream_health: &StreamHealthRegistry,
    streams: &StreamRegistry,
    bus: &EventBus,
    quote_tx: mpsc::Sender<dataprovider::Quote>,
) {
    streams.abort_and_remove("databento-opra").await;
    let health = stream_health.handle("DATABENTO", "OPRA", StreamKind::Feed);
    health.set_connecting();
    let session = QuoteFeedSession::new(
        DatabentoOpraFeed::new(api_key),
        pool,
        quote_tx,
        bus.subscribe_order(),
        health.clone(),
    );
    // `supervise` returns `!` (it never returns), so `JoinHandle<!>` — wrap it in
    // a block so the spawned future's output is `()`, matching what
    // `StreamRegistry` stores. The wrapping changes nothing about behavior:
    // `.await` on a `!`-returning future never completes either way.
    let handle = tokio::spawn(async move {
        stream_supervisor::supervise("DATABENTO/OPRA", health, session).await;
    });
    streams.insert("databento-opra", handle);
}

pub(crate) use crate::connectors::exec_stream_code;

/// The network boundary of reload. Store reads, classification, serialization,
/// swaps and stops remain real in integration tests; only new external sessions
/// are substituted with controlled tasks.
#[async_trait::async_trait]
pub(crate) trait ReloadStreams: Send + Sync + 'static {
    async fn exec(&self, stream: ExecStreamStart, deps: &RegistrationDeps, streams: &StreamRegistry);

    async fn databento(&self, api_key: String, state: &AppState);
}

pub(crate) struct LiveReloadStreams;

#[async_trait::async_trait]
impl ReloadStreams for LiveReloadStreams {
    async fn exec(&self, stream: ExecStreamStart, deps: &RegistrationDeps, streams: &StreamRegistry) {
        restart_exec_stream(&stream, deps, streams).await;
    }

    async fn databento(&self, api_key: String, state: &AppState) {
        restart_databento_feed(api_key, state.pool().clone(), state.stream_health(),
            state.streams(), state.event_bus(), state.quote_tx()).await;
    }
}

/// (Re)start one connection's execution stream — the task that delivers its
/// fills — against the adapter just registered, under `exec_stream_code`.
///
/// Every call to `build_registry` builds a brand new adapter for each active,
/// configured connection it does not carry forward, and the stream books
/// against that adapter, so it must follow it every time: skipping the respawn
/// would leave fills landing against an adapter the registry no longer routes
/// orders through. Aborting first (rather than only inserting) is what makes
/// boot — where nothing runs yet — the same call as a reload.
pub async fn restart_exec_stream(stream: &ExecStreamStart, deps: &RegistrationDeps, streams: &StreamRegistry) {
    let code = exec_stream_code(stream.code, stream.env);
    streams.abort_and_remove(&code).await;
    let health = deps.stream_health.handle(stream.code, stream.env, StreamKind::Execution);
    health.set_connecting();
    let handle = tokio::spawn((stream.start)(health, deps.engine.clone()));
    streams.insert(code, handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::alpaca::AlpacaAdapter;
    use crate::credentials::{BrokerCredentials, Connection, CredentialState};

    fn conn(code: &str, status: &str, state: CredentialState<BrokerCredentials>) -> Connection<BrokerCredentials> {
        Connection {
            code: code.into(),
            kind: "ALPACA".into(),
            environment: Some("PAPER".into()),
            status: status.into(),
            credentials: state,
            credentials_updated_at: None,
        }
    }

    fn alpaca() -> BrokerCredentials {
        BrokerCredentials::from_typed("Alpaca", &crate::adapters::alpaca::AlpacaCredentials { key: "k".into(), secret: "s".into() })
    }

    fn ibkr() -> BrokerCredentials {
        BrokerCredentials::from_typed("IbkrFix", &crate::fix::ibkr::IbkrFixCredentials {
            host: "h".into(), port: 4001,
            sender_comp_id: "OMS".into(), target_comp_id: "IBKR".into(),
            password: "p".into(), ssl: true,
        })
    }

    fn binance() -> BrokerCredentials {
        BrokerCredentials::from_typed("BinanceFix", &crate::adapters::binance::BinanceCredentials {
            host: "h".into(), port: 9000,
            sender_comp_id: "OMS".into(), target_comp_id: "SPOT".into(),
            api_key: "k".into(), private_key: "pk".into(),
        })
    }

    /// A `RegistrationDeps` that never touches the network: `PgPool::connect_lazy`
    /// builds a pool without ever connecting (see `app_state.rs`'s tests), and the
    /// `RestartRequired` carry-forward path this is used with never dials out —
    /// it only reads `current`.
    fn stub_deps() -> RegistrationDeps {
        RegistrationDeps {
            stream_health: StreamHealthRegistry::new(),
            engine: gateway::contract::EngineHandle::new().0,
        }
    }

    /// The `StreamRegistry` keys as the per-broker functions spelled them before
    /// connectors: a reload must find and replace streams started by a build
    /// from before the change.
    #[test]
    fn exec_stream_codes_are_unchanged() {
        assert_eq!(exec_stream_code("ALPACA", "PAPER"), "alpaca-paper:exec");
        assert_eq!(exec_stream_code("BINANCE", "LIVE"), "binance-live:exec");
        assert_eq!(exec_stream_code("BYBIT", "PAPER"), "bybit-paper:exec");
        assert_eq!(exec_stream_code("COINBASE", "LIVE"), "coinbase-live:exec");
        assert_eq!(exec_stream_code("OKX", "PAPER"), "okx-paper:exec");
    }

    #[tokio::test]
    async fn unknown_credential_kind_is_reported_failed() {
        let c = conn("nasdaq-paper", "ACTIVE", CredentialState::Configured(BrokerCredentials::new("Nasdaq", Default::default())));
        let out = build_registry(&[c], true, &BrokerRegistry::new(), &stub_deps()).await;
        assert!(matches!(&out.report.connections[0].1, ConnectionOutcome::Failed(r) if r.contains("Nasdaq")));
        assert!(out.exec_streams.is_empty());
    }

    /// Every REST connector's execution stream comes back to the caller to
    /// (re)start; FIX connectors run their own session and hand back none.
    #[tokio::test]
    async fn every_rest_connector_hands_back_an_exec_stream() {
        let mut coinbase = coinbase_conn();
        coinbase.environment = Some("LIVE".into());
        let mut bybit = okx_conn();
        bybit.kind = "BYBIT".into();
        bybit.credentials = CredentialState::Configured(BrokerCredentials::from_typed(
            "Bybit",
            &crate::adapters::bybit::BybitCredentials { api_key: "k".into(), api_secret: "s".into() },
        ));
        let connections = [conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca())), bybit, coinbase, okx_conn()];
        let out = build_registry(&connections, true, &BrokerRegistry::new(), &stub_deps()).await;
        let mut codes: Vec<&str> = out.exec_streams.iter().map(|s| s.code).collect();
        codes.sort_unstable();
        assert_eq!(codes, ["ALPACA", "BYBIT", "COINBASE", "OKX"]);
    }

    fn ibkr_conn(code: &str, status: &str, credentials: CredentialState<BrokerCredentials>) -> Connection<BrokerCredentials> {
        Connection {
            code: code.into(),
            kind: "IBKR".into(),
            environment: Some("PAPER".into()),
            status: status.into(),
            credentials,
            credentials_updated_at: None,
        }
    }

    /// Any concrete `BrokerAdapter` stands in for "an adapter is already running"
    /// — `build_registry` only ever moves the `Arc` around for the carry-forward
    /// path, it never inspects what is inside it.
    fn stub_adapter() -> Arc<AlpacaAdapter> {
        Arc::new(AlpacaAdapter::new("k".into(), "s".into(), "PAPER"))
    }

    #[test]
    fn a_disabled_connection_is_never_registered() {
        let c = conn("alpaca-paper", "DISABLED", CredentialState::Configured(alpaca()));
        assert!(matches!(classify(&c, true), ConnectionOutcome::Disabled));
        assert!(matches!(classify(&c, false), ConnectionOutcome::Disabled));
    }

    #[test]
    fn an_unconfigured_connection_is_reported_not_registered() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Unconfigured);
        assert!(matches!(classify(&c, false), ConnectionOutcome::Unconfigured));
    }

    /// An undecryptable credential must be surfaced, never silently skipped —
    /// the operator needs to know the difference between "not set up" and
    /// "set up but the key is wrong".
    #[test]
    fn an_error_credential_is_reported_with_its_reason() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        match classify(&c, false) {
            ConnectionOutcome::Failed(msg) => assert!(msg.contains("bad key")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The claim this whole plan rests on: a REST credential can be swapped in a
    /// running process, a FIX one cannot, because the session thread parks
    /// forever with no stop path.
    #[test]
    fn fix_connections_need_a_restart_but_rest_ones_do_not() {
        let rest = conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca()));
        let ibkr_fix = conn("ibkr-paper", "ACTIVE", CredentialState::Configured(ibkr()));
        let binance_fix = conn("binance-paper", "ACTIVE", CredentialState::Configured(binance()));

        // At boot both are registered — nothing is running yet to conflict with.
        assert!(matches!(classify(&rest, true), ConnectionOutcome::Registered));
        assert!(matches!(classify(&ibkr_fix, true), ConnectionOutcome::Registered));
        assert!(matches!(classify(&binance_fix, true), ConnectionOutcome::Registered));

        // On reload the FIX ones must report a restart rather than starting a
        // second session to the same venue — including `BinanceFix` when the
        // actual transport would have been REST (see `classify`'s doc comment).
        assert!(matches!(classify(&rest, false), ConnectionOutcome::Registered));
        assert!(matches!(classify(&ibkr_fix, false), ConnectionOutcome::RestartRequired));
        assert!(matches!(classify(&binance_fix, false), ConnectionOutcome::RestartRequired));
    }

    /// The most consequential rule in this plan, pinned end-to-end through
    /// `build_registry` rather than asserted only in a comment: a reload must
    /// never disarm a FIX session that is already running. `current` holds an
    /// adapter under ("IBKR", "PAPER"); the connection classifies as
    /// `RestartRequired` (is_boot = false), and that existing adapter must land
    /// in the new registry untouched — `fix::start_ibkr` must never be called
    /// (it would try to dial out, which `stub_deps`'s lazy pool can't satisfy
    /// anyway, but the point is it must not even be attempted).
    #[tokio::test]
    async fn restart_required_carries_the_existing_fix_adapter_forward() {
        let mut current = BrokerRegistry::new();
        current.register("IBKR", "PAPER", stub_adapter());

        let c = ibkr_conn("ibkr-paper", "ACTIVE", CredentialState::Configured(ibkr()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(output.registry.get("IBKR", "PAPER").is_some(), "the running adapter must be carried forward, not dropped");
        assert_eq!(
            output.report.connections,
            vec![("ibkr-paper".to_string(), ConnectionOutcome::RestartRequired)]
        );
    }

    /// The flip side: a connection that is not eligible to run at all — disabled,
    /// or never configured — must not inherit whatever happens to be sitting in
    /// `current` under the same (kind, environment) key. The control flow makes
    /// this impossible today (both cases return before the carry-forward branch
    /// is even reached); this pins it so it stays impossible.
    #[tokio::test]
    async fn disabled_and_unconfigured_connections_never_carry_an_adapter_forward() {
        let mut current = BrokerRegistry::new();
        current.register("IBKR", "PAPER", stub_adapter());

        let disabled = ibkr_conn("ibkr-paper", "DISABLED", CredentialState::Configured(ibkr()));
        let output = build_registry(&[disabled], false, &current, &stub_deps()).await;
        assert!(output.registry.get("IBKR", "PAPER").is_none());

        let unconfigured = ibkr_conn("ibkr-paper", "ACTIVE", CredentialState::Unconfigured);
        let output = build_registry(&[unconfigured], false, &current, &stub_deps()).await;
        assert!(output.registry.get("IBKR", "PAPER").is_none());
    }

    /// A `RestartRequired` connection with nothing in `current` to carry forward
    /// (e.g. a brand new FIX credential added between boot and this reload) must
    /// not silently claim `RestartRequired` while registering nothing — that
    /// reads as "restart to pick up your change" when there is no fallback
    /// adapter serving the connection at all right now. It must downgrade to
    /// `Failed` with a reason naming both facts.
    #[tokio::test]
    async fn restart_required_with_nothing_to_carry_forward_is_reported_failed() {
        let current = BrokerRegistry::new(); // empty: nothing running for any venue
        let c = ibkr_conn("ibkr-paper", "ACTIVE", CredentialState::Configured(ibkr()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(output.registry.get("IBKR", "PAPER").is_none());
        match &output.report.connections[0] {
            (code, ConnectionOutcome::Failed(reason)) => {
                assert_eq!(code, "ibkr-paper");
                assert!(reason.contains("restart"), "reason should say a restart is needed: {reason}");
                assert!(reason.contains("no running adapter"), "reason should say there is nothing to fall back on: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The same rule as `restart_required_carries_the_existing_fix_adapter_forward`,
    /// pinned for the other way an adapter can go missing: the *stored* row
    /// itself became unreadable rather than needing a restart. A single bad
    /// row (a `rotate-key` run racing this reload, a hand-edited row, bit rot)
    /// must not be able to disarm a connection that was routing orders a
    /// moment ago — `current` still holds a good adapter built the last time
    /// this decrypted, and it must be carried forward exactly like
    /// `RestartRequired`'s adapter is, while the report still says `Failed`
    /// so the operator is told.
    #[tokio::test]
    async fn an_error_credential_carries_the_existing_adapter_forward() {
        let mut current = BrokerRegistry::new();
        current.register("IBKR", "PAPER", stub_adapter());

        let c = ibkr_conn("ibkr-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(
            output.registry.get("IBKR", "PAPER").is_some(),
            "the previously-working adapter must be carried forward despite the row failing to decrypt"
        );
        match &output.report.connections[0] {
            (code, ConnectionOutcome::Failed(reason)) => {
                assert_eq!(code, "ibkr-paper");
                assert!(reason.contains("bad key"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The same rule for a connection with its own execution stream: the
    /// carried adapter must be the one the stream keeps booking against.
    #[tokio::test]
    async fn an_error_alpaca_credential_carries_its_adapter_forward() {
        let mut current = BrokerRegistry::new();
        current.register("ALPACA", "PAPER", stub_adapter());

        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(
            output.registry.get("ALPACA", "PAPER").is_some(),
            "the carried-forward adapter must still be registered"
        );
    }

    /// The flip side of both tests above: an `Error`ed connection with nothing
    /// in `current` to carry forward has nothing to register — it must not
    /// panic or fabricate an adapter, just report `Failed` with the decrypt
    /// error's own reason.
    #[tokio::test]
    async fn an_error_credential_with_nothing_to_carry_forward_registers_nothing() {
        let current = BrokerRegistry::new();
        let c = ibkr_conn("ibkr-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(output.registry.get("IBKR", "PAPER").is_none());
        assert!(matches!(&output.report.connections[0], (_, ConnectionOutcome::Failed(_))));
    }

    /// Swapping an Alpaca adapter must also restart its execution stream, or
    /// orders route on the new credential while fills arrive on the old one.
    #[tokio::test]
    async fn reloading_alpaca_also_restarts_its_execution_stream() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca()));
        let out = build_registry(&[c], false, &BrokerRegistry::new(), &stub_deps()).await;
        assert_eq!(out.exec_streams.iter().map(|s| s.code).collect::<Vec<_>>(), ["ALPACA"]);

        let disabled = conn("alpaca-paper", "DISABLED", CredentialState::Configured(alpaca()));
        let out = build_registry(&[disabled], false, &BrokerRegistry::new(), &stub_deps()).await;
        assert!(out.exec_streams.is_empty());
    }

    /// The execution stream's registry key must never collide with a feed
    /// code — `restart_databento_feed` keys the same `StreamRegistry` by bare
    /// connection code ("databento-opra"), so an execution stream's key must
    /// be shaped so it can never equal one, for any environment.
    #[test]
    fn a_bybit_credential_applies_on_reload_without_a_restart() {
        let c = conn(
            "bybit-paper",
            "ACTIVE",
            CredentialState::Configured(BrokerCredentials::from_typed("Bybit", &crate::adapters::bybit::BybitCredentials { api_key: "k".into(), api_secret: "s".into() })),
        );
        assert!(matches!(classify(&c, false), ConnectionOutcome::Registered));
        assert!(matches!(classify(&c, true), ConnectionOutcome::Registered));
    }

    fn coinbase_conn() -> Connection<BrokerCredentials> {
        let pem = p256::SecretKey::random(&mut rand::thread_rng())
            .to_sec1_pem(p256::pkcs8::LineEnding::LF)
            .unwrap()
            .to_string();
        let mut c = conn(
            "coinbase-live",
            "ACTIVE",
            CredentialState::Configured(BrokerCredentials::from_typed("Coinbase", &crate::adapters::coinbase::CoinbaseCredentials { api_key: "organizations/o/apiKeys/k".into(), private_key: pem })),
        );
        c.environment = Some("LIVE".into());
        c
    }

    #[test]
    fn a_coinbase_credential_applies_on_reload_without_a_restart() {
        assert!(matches!(classify(&coinbase_conn(), false), ConnectionOutcome::Registered));
    }

    #[tokio::test]
    async fn coinbase_credentials_on_a_paper_connection_are_refused() {
        let mut c = coinbase_conn();
        c.environment = Some("PAPER".into());
        let deps = RegistrationDeps {
            stream_health: StreamHealthRegistry::new(),
            engine: gateway::contract::EngineHandle::new().0,
        };
        let out = build_registry(&[c], false, &BrokerRegistry::new(), &deps).await;
        assert!(out.registry.get("COINBASE", "PAPER").is_none());
        assert!(!out.exec_streams.iter().any(|s| s.code == "COINBASE"));
        assert!(matches!(&out.report.connections[0].1, ConnectionOutcome::Failed(m) if m.contains("LIVE")));
    }

    #[test]
    fn coinbase_exec_stream_code_is_its_own() {
        assert_eq!(exec_stream_code("COINBASE", "LIVE"), "coinbase-live:exec");
        assert_ne!(exec_stream_code("COINBASE", "LIVE"), exec_stream_code("BYBIT", "LIVE"));
    }

    #[tokio::test]
    async fn a_configured_coinbase_connection_is_registered_and_handed_back() {
        let deps = RegistrationDeps {
            stream_health: StreamHealthRegistry::new(),
            engine: gateway::contract::EngineHandle::new().0,
        };
        let out = build_registry(&[coinbase_conn()], false, &BrokerRegistry::new(), &deps).await;
        assert!(out.registry.get("COINBASE", "LIVE").is_some());
        assert_eq!(out.exec_streams.iter().filter(|s| s.code == "COINBASE").map(|s| s.env).collect::<Vec<_>>(), ["LIVE"]);
    }

    fn okx_conn() -> Connection<BrokerCredentials> {
        conn(
            "okx-paper",
            "ACTIVE",
            CredentialState::Configured(BrokerCredentials::from_typed("Okx", &crate::adapters::okx::OkxCredentials { api_key: "k".into(), api_secret: "s".into(), passphrase: "p".into() })),
        )
    }

    #[test]
    fn an_okx_credential_applies_on_reload_without_a_restart() {
        assert!(matches!(classify(&okx_conn(), false), ConnectionOutcome::Registered));
    }

    #[test]
    fn okx_exec_stream_code_is_per_environment() {
        assert_eq!(exec_stream_code("OKX", "PAPER"), "okx-paper:exec");
        assert_eq!(exec_stream_code("OKX", "LIVE"), "okx-live:exec");
    }

    #[tokio::test]
    async fn a_configured_okx_connection_is_registered_and_handed_back_for_its_stream() {
        let deps = RegistrationDeps {
            stream_health: StreamHealthRegistry::new(),
            engine: gateway::contract::EngineHandle::new().0,
        };
        let out = build_registry(&[okx_conn()], false, &BrokerRegistry::new(), &deps).await;
        assert!(out.registry.get("OKX", "PAPER").is_some());
        assert_eq!(out.exec_streams.iter().filter(|s| s.code == "OKX").map(|s| s.env).collect::<Vec<_>>(), ["PAPER"]);
    }

    #[test]
    fn bybit_exec_stream_code_is_per_environment() {
        assert_eq!(exec_stream_code("BYBIT", "PAPER"), "bybit-paper:exec");
        assert_eq!(exec_stream_code("BYBIT", "LIVE"), "bybit-live:exec");
        assert_ne!(exec_stream_code("BYBIT", "PAPER"), exec_stream_code("BINANCE", "PAPER"));
    }

    #[tokio::test]
    async fn a_configured_bybit_connection_is_registered_and_handed_back_for_its_stream() {
        let c = conn(
            "bybit-paper",
            "ACTIVE",
            CredentialState::Configured(BrokerCredentials::from_typed("Bybit", &crate::adapters::bybit::BybitCredentials { api_key: "k".into(), api_secret: "s".into() })),
        );
        let deps = RegistrationDeps {
            stream_health: StreamHealthRegistry::new(),
            engine: gateway::contract::EngineHandle::new().0,
        };
        let out = build_registry(&[c], false, &BrokerRegistry::new(), &deps).await;
        assert!(out.registry.get("BYBIT", "PAPER").is_some());
        assert_eq!(out.exec_streams.iter().filter(|s| s.code == "BYBIT").map(|s| s.env).collect::<Vec<_>>(), ["PAPER"]);
    }

    #[test]
    fn alpaca_exec_stream_code_cannot_collide_with_a_feed_code() {
        assert_eq!(exec_stream_code("ALPACA", "PAPER"), "alpaca-paper:exec");
        assert_eq!(exec_stream_code("ALPACA", "LIVE"), "alpaca-live:exec");
        assert_ne!(exec_stream_code("ALPACA", "PAPER"), "databento-opra");
    }

    /// The report is an HTTP response body. It must name connections and
    /// outcomes and nothing else: a reload that echoed a credential would undo
    /// the entire point of encrypting it.
    #[test]
    fn the_report_carries_no_credential_material() {
        let report = ReloadReport {
            connections: vec![
                ("alpaca-paper".into(), ConnectionOutcome::Registered),
                ("ibkr-paper".into(), ConnectionOutcome::RestartRequired),
                ("binance-paper".into(), ConnectionOutcome::Failed("could not decrypt".into())),
            ],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        for secret in ["SUPERSECRET", "BEGIN PRIVATE KEY", "api_key"] {
            assert!(!json.contains(secret), "{secret} in {json}");
        }
        assert!(json.contains("alpaca-paper") && json.contains("RestartRequired"));
    }
}
