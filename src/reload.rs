//! Turning the credential store into live adapters — at boot and again on demand.
//!
//! Boot and reload share this module so a credential registered at startup and
//! one registered at runtime cannot diverge. The split that matters is which
//! connections can be rebuilt in a running process at all: a REST adapter is an
//! HTTP client and swapping it is free, while a FIX session owns an OS thread
//! that parks forever with no stop path (`fix::start_session`), so replacing one
//! would mean a second session logging in against the first.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::adapters::alpaca::AlpacaAdapter;
use crate::alpaca_stream;
use crate::adapters::binance::BinanceAdapter;
use crate::adapters::{BrokerRegistry, Transport};
use crate::app_state::{DoorbellRegistry, StreamRegistry};
use crate::credentials::{BrokerCredentials, Connection, CredentialState};
use crate::fix;
use crate::kafka::KafkaClient;
use crate::opra_stream::DatabentoOpraFeed;
use crate::quote_feed::QuoteFeedSession;
use crate::stream_health::{StreamHealthRegistry, StreamKind};
use crate::stream_supervisor;

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
        CredentialState::Configured(c) => match c {
            BrokerCredentials::Alpaca { .. } => ConnectionOutcome::Registered,
            // `BinanceFix` covers both transports — the REST path is selected at
            // registration time by `Transport::from_env`, not by which variant the
            // credential is. Reporting `RestartRequired` here even when the actual
            // transport turns out to be REST is deliberately conservative: a
            // Binance credential change needs a restart even on REST, for now.
            // Narrowing that is a follow-up, not this task.
            BrokerCredentials::IbkrFix { .. } | BrokerCredentials::BinanceFix { .. } => {
                if is_boot {
                    ConnectionOutcome::Registered
                } else {
                    ConnectionOutcome::RestartRequired
                }
            }
        },
    }
}

/// True when reloading this connection also means restarting its execution
/// stream — the task that delivers fills, separate from the adapter that
/// routes orders. Only Alpaca has one: its trade-update stream is a plain
/// websocket task spawned alongside the adapter, signing its own auth with a
/// copy of the credential (`src/alpaca_stream.rs`). A FIX broker's execution
/// reports arrive over the same session that routes its orders, so there is
/// nothing separate to restart, and `classify` never reports `Registered` for
/// one on reload anyway (see its doc comment) — so in practice this is only
/// ever asked about a connection already known to be Alpaca.
///
/// Called by the `/admin/connections/reload` handler (`admin.rs`) to decide,
/// per connection, whether to restart its execution stream — rather than the
/// handler re-deriving the same answer from `alpaca_creds` membership, which
/// would be a second, potentially diverging judgment call on the same
/// question `classify` already settled.
pub fn restarts_execution_stream(outcome: &ConnectionOutcome) -> bool {
    matches!(outcome, ConnectionOutcome::Registered)
}

/// What the FIX starters and freshly-built adapters need beyond the credential
/// itself. Built once in `serve()` and handed to every call of `build_registry`
/// — boot today, a runtime reload endpoint in a later task — so both draw on the
/// exact same pool, stream-health registry, Kafka client and fill-doorbell
/// sender rather than each constructing (or re-deriving) their own.
pub struct RegistrationDeps {
    pub pool: PgPool,
    pub stream_health: StreamHealthRegistry,
    pub kafka: Option<KafkaClient>,
    pub position_changed_tx: Option<mpsc::Sender<()>>,
}

/// Everything `build_registry` produces. `registry` and `report` are the pair
/// the brief for this task describes; `alpaca_creds` and `binance_rest_adapters`
/// are carried alongside them for the same reason they were stashed inline in
/// the old `serve()` loop — the caller still needs to spawn each REST adapter's
/// execution-report stream, and that needs either the raw (key, secret) pair
/// (Alpaca signs its websocket auth message with them directly — the adapter
/// keeps them private) or the concrete adapter type (`BrokerRegistry::get`
/// erases to `Arc<dyn BrokerAdapter>`, and there is no `get_binance` the way
/// there is `get_alpaca`). Neither belongs on `ReloadReport`: that type is
/// serialised straight into an HTTP response in Task 5, and one holds secrets.
///
/// Trap for whoever wires the reload endpoint: `alpaca_creds` is rebuilt in
/// full on every call, including a reload where nothing about that Alpaca
/// connection changed — Alpaca is never `RestartRequired`, so it is always
/// re-registered fresh. `binance_rest_adapters`, by contrast, comes back
/// *empty* for a Binance connection that classified as `RestartRequired` and
/// was carried forward (see `build_registry`) — no new adapter was built, so
/// there is nothing to add to the list. A handler that naively respawns a
/// trade-update stream for every entry in `alpaca_creds` on every reload will
/// start a second, redundant Alpaca stream alongside the one still running
/// from boot (or the previous reload); the corresponding Binance stream, if
/// coded the same way, would simply — and correctly — not be respawned.
pub struct RegistrationOutput {
    pub registry: BrokerRegistry,
    pub report: ReloadReport,
    /// (key, secret) for each Alpaca environment actually registered this pass.
    pub alpaca_creds: HashMap<&'static str, (String, String)>,
    /// REST-transport Binance adapters actually registered this pass, concrete
    /// type, in registration order.
    pub binance_rest_adapters: Vec<(&'static str, Arc<BinanceAdapter>)>,
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
    let mut alpaca_creds: HashMap<&'static str, (String, String)> = HashMap::new();
    let mut binance_rest_adapters: Vec<(&'static str, Arc<BinanceAdapter>)> = Vec::new();
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
                //
                // Alpaca goes through `register_alpaca`/`get_alpaca`, not the
                // generic path: `BrokerRegistry` keeps a second, Alpaca-only
                // map that `restart_alpaca_stream` (via `get_alpaca`) reads
                // from, and only `register_alpaca` keeps both maps in sync.
                if conn.kind == "ALPACA" {
                    if let Some(adapter) = current.get_alpaca(env_name) {
                        registry.register_alpaca(env_name, adapter);
                    }
                } else if let Some(adapter) = current.get(&conn.kind, env_name) {
                    registry.register(&conn.kind, env_name, adapter);
                }
            }
            CredentialState::Configured(creds) => {
                // `broker_code` and the stored credential's own variant tag are two
                // independent sources of truth for "what kind of broker is this" —
                // normally in lockstep, but nothing enforces it (e.g. a credential
                // hand-imported into the wrong row). Registration below follows the
                // credential's variant, not `conn.kind`, so this cannot mis-route an
                // adapter — but the mismatch itself is a real misconfiguration worth
                // surfacing rather than registering silently.
                let expected_kind = match creds {
                    BrokerCredentials::Alpaca { .. } => "ALPACA",
                    BrokerCredentials::IbkrFix { .. } => "IBKR",
                    BrokerCredentials::BinanceFix { .. } => "BINANCE",
                };
                if conn.kind != expected_kind {
                    warn!(
                        code = %conn.code, kind = %conn.kind, credential_kind = expected_kind,
                        "broker_connection's broker_code does not match its stored credential's kind"
                    );
                }
                match creds {
                    BrokerCredentials::Alpaca { key, secret } => {
                        registry.register_alpaca(env_name, Arc::new(AlpacaAdapter::new(key.clone(), secret.clone(), env_name)));
                        alpaca_creds.insert(env_name, (key.clone(), secret.clone()));
                        info!(code = %conn.code, credentials_updated_at = ?conn.credentials_updated_at, "registered ALPACA/{env_name} adapter");
                    }
                    BrokerCredentials::IbkrFix { .. } => {
                        // IBKR is FIX-only — the FIX session both routes orders and
                        // delivers execution reports.
                        if matches!(outcome, ConnectionOutcome::RestartRequired) {
                            // Carry the running session forward rather than starting a
                            // second one against the same venue (see fn doc comment).
                            match current.get("IBKR", env_name) {
                                Some(adapter) => registry.register("IBKR", env_name, adapter),
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
                            match fix::start_ibkr(
                                env_name,
                                creds,
                                &deps.stream_health,
                                deps.pool.clone(),
                                deps.kafka.clone(),
                                deps.position_changed_tx.clone(),
                            ) {
                                Some(adapter) => registry.register("IBKR", env_name, adapter),
                                None => {
                                    outcome = ConnectionOutcome::Failed("IBKR FIX session could not start".into());
                                }
                            }
                        }
                    }
                    BrokerCredentials::BinanceFix { api_key, private_key, .. } => {
                        if matches!(outcome, ConnectionOutcome::RestartRequired) {
                            // Conservative on purpose (see `classify`): carry forward
                            // whatever is currently registered, FIX or REST, rather
                            // than rebuilding — even a REST rebuild is deferred to a
                            // follow-up, not this task.
                            match current.get("BINANCE", env_name) {
                                Some(adapter) => registry.register("BINANCE", env_name, adapter),
                                None => {
                                    outcome = ConnectionOutcome::Failed(
                                        "connection needs a restart to apply this change and has no running adapter to fall back on".into(),
                                    );
                                }
                            }
                        } else {
                            // Transport is an explicit choice via BINANCE_{ENV}_TRANSPORT=fix|rest
                            // (default rest) — a wire-protocol setting, not a secret, so it stays
                            // on the environment. `fix` runs one FIX session for order entry +
                            // execution reports; `rest` runs the REST adapter + WS user-data stream,
                            // built from the same store credential (no more PEM file read).
                            match Transport::from_env(&format!("BINANCE_{env_name}"), Transport::Rest) {
                                Transport::Fix => {
                                    match fix::start_binance(
                                        env_name,
                                        creds,
                                        &deps.stream_health,
                                        deps.pool.clone(),
                                        deps.kafka.clone(),
                                        deps.position_changed_tx.clone(),
                                    ) {
                                        Some(adapter) => registry.register("BINANCE", env_name, adapter),
                                        None => {
                                            error!(code = %conn.code, "Binance FIX session could not start");
                                            outcome = ConnectionOutcome::Failed("Binance FIX session could not start".into());
                                        }
                                    }
                                }
                                Transport::Rest => match BinanceAdapter::new(api_key.clone(), private_key, env_name) {
                                    Ok(adapter) => {
                                        let adapter = Arc::new(adapter);
                                        registry.register("BINANCE", env_name, adapter.clone());
                                        binance_rest_adapters.push((env_name, adapter));
                                        info!(code = %conn.code, credentials_updated_at = ?conn.credentials_updated_at, "registered BINANCE/{env_name} adapter (REST/WS)");
                                    }
                                    Err(e) => {
                                        error!(code = %conn.code, "Binance adapter not registered: {e}");
                                        outcome = ConnectionOutcome::Failed(format!("Binance adapter not registered: {e}"));
                                    }
                                },
                            }
                        }
                    }
                }
            }
        }
        report.connections.push((conn.code.clone(), outcome));
    }

    RegistrationOutput { registry, report, alpaca_creds, binance_rest_adapters }
}

/// (Re)start the Databento OPRA feed session under (possibly new) credentials,
/// publishing the fresh handle into `streams` and the fresh doorbell sender
/// into `doorbells`, both under the same code ("databento-opra") this feed
/// always runs under.
///
/// Only Databento is credential-driven among the market-data feeds — Binance
/// and Bybit are public and are never restarted here or anywhere else (see
/// `StreamRegistry`'s doc comment in `app_state.rs`). That is also why this
/// function, unlike `build_registry`, does not loop over a slice of
/// connections: there is exactly one credentialed feed to restart.
///
/// Calling this when nothing is registered yet (boot) is safe: `abort_and_remove`
/// on an empty registry is a no-op and `doorbells.set` on an unpopulated code
/// is a plain insert, so boot and a later credential-driven restart are the
/// same call. The doorbell channel pair is created *inside* this function
/// (rather than threaded in by the caller) precisely so that guarantee holds:
/// a caller that built its own pair and pushed the sender into a fan-out list
/// by hand could forget to replace the old entry, which is the bug this
/// function exists to make impossible — see `DoorbellRegistry`'s doc comment.
pub fn restart_databento_feed(
    api_key: String,
    pool: PgPool,
    stream_health: &StreamHealthRegistry,
    streams: &StreamRegistry,
    doorbells: &DoorbellRegistry,
    quote_tx: mpsc::Sender<dataprovider::Quote>,
) {
    streams.abort_and_remove("databento-opra");
    let (position_changed_tx, position_changed_rx) = mpsc::channel::<()>(1);
    doorbells.set("databento-opra", position_changed_tx);
    let health = stream_health.handle("DATABENTO", "OPRA", StreamKind::Feed);
    let session = QuoteFeedSession::new(
        DatabentoOpraFeed::new(api_key),
        pool,
        quote_tx,
        position_changed_rx,
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

/// The `StreamRegistry` key for one environment's Alpaca execution stream.
///
/// Feed codes are connection codes ("databento-opra"); this is deliberately
/// not one — the ":exec" suffix a connection code can never contain — so an
/// execution stream can never collide with a feed entry in the same registry,
/// and aborting one can never accidentally take out the other.
///
/// `pub(crate)`, not private: `admin::reload_connections` also needs this key
/// to stop a stream whose connection is no longer registered (Disabled,
/// Unconfigured, or Failed), not only to start one.
pub(crate) fn alpaca_exec_stream_code(env_name: &str) -> String {
    format!("alpaca-{}:exec", env_name.to_lowercase())
}

/// (Re)start one Alpaca environment's execution-report stream — the task that
/// delivers fills — against a freshly built `adapter`, registering its handle
/// in `streams` under `alpaca_exec_stream_code` (see there for why that key,
/// not the connection code).
///
/// Unlike `restart_databento_feed`, there is no "nothing changed, skip it"
/// case to consider: every call to `build_registry` builds a brand new
/// `AlpacaAdapter` for each active, configured Alpaca connection (`classify`
/// never reports `RestartRequired` for Alpaca), so this must run every time
/// that happens. The stream signs its own websocket auth directly with `key`
/// and `secret`, and holds `adapter` to post fills back through — both fresh
/// on every call — so skipping the respawn would leave the previous stream's
/// fills landing against an adapter the registry no longer serves orders
/// through. That split is exactly what this task exists to close; aborting
/// first (rather than only inserting) is what makes a reload also cover the
/// case where nothing is running yet, same as `restart_databento_feed`.
///
/// Takes `deps` rather than its individual fields (pool, stream_health, kafka,
/// position_changed_tx) both to stay under clippy's argument-count lint and
/// because it is the same bundle `build_registry` already draws on — boot and
/// a later reload share one source for these rather than each re-deriving or
/// threading them through separately.
pub fn restart_alpaca_stream(
    env_name: &'static str,
    key: String,
    secret: String,
    adapter: Arc<AlpacaAdapter>,
    deps: &RegistrationDeps,
    streams: &StreamRegistry,
) {
    let code = alpaca_exec_stream_code(env_name);
    streams.abort_and_remove(&code);
    let health = deps.stream_health.handle("ALPACA", env_name, StreamKind::Execution);
    let handle = tokio::spawn(alpaca_stream::run(
        env_name,
        key,
        secret,
        deps.pool.clone(),
        deps.kafka.clone(),
        adapter,
        health,
        deps.position_changed_tx.clone(),
    ));
    streams.insert(code, handle);
}

#[cfg(test)]
mod tests {
    use super::*;
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
        BrokerCredentials::Alpaca { key: "k".into(), secret: "s".into() }
    }

    fn ibkr() -> BrokerCredentials {
        BrokerCredentials::IbkrFix {
            host: "h".into(), port: 4001,
            sender_comp_id: "OMS".into(), target_comp_id: "IBKR".into(),
            password: "p".into(), ssl: true,
        }
    }

    fn binance() -> BrokerCredentials {
        BrokerCredentials::BinanceFix {
            host: "h".into(), port: 9000,
            sender_comp_id: "OMS".into(), target_comp_id: "SPOT".into(),
            api_key: "k".into(), private_key: "pk".into(),
        }
    }

    /// A `RegistrationDeps` that never touches the network: `PgPool::connect_lazy`
    /// builds a pool without ever connecting (see `app_state.rs`'s tests), and the
    /// `RestartRequired` carry-forward path this is used with never dials out —
    /// it only reads `current`.
    fn stub_deps() -> RegistrationDeps {
        RegistrationDeps {
            pool: PgPool::connect_lazy("postgres://localhost/oms_test_never_connects")
                .expect("connect_lazy never actually connects"),
            stream_health: StreamHealthRegistry::new(),
            kafka: None,
            position_changed_tx: None,
        }
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

    /// The Alpaca-specific half of the same rule: `BrokerRegistry` keeps a
    /// second, Alpaca-only map (`get_alpaca`) that only `register_alpaca`
    /// keeps in sync with the generic one — carrying forward through the
    /// generic `register`/`get` alone would leave `get_alpaca` unable to find
    /// the carried adapter, which is what `restart_alpaca_stream` (and a
    /// later reload that *does* decrypt fine) actually calls.
    #[tokio::test]
    async fn an_error_alpaca_credential_carries_forward_through_get_alpaca() {
        let mut current = BrokerRegistry::new();
        current.register_alpaca("PAPER", stub_adapter());

        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        let output = build_registry(&[c], false, &current, &stub_deps()).await;

        assert!(
            output.registry.get_alpaca("PAPER").is_some(),
            "get_alpaca must still resolve the carried-forward adapter, not just the generic get()"
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
    #[test]
    fn reloading_alpaca_also_restarts_its_execution_stream() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca()));
        assert!(restarts_execution_stream(&classify(&c, false)));

        let disabled = conn("alpaca-paper", "DISABLED", CredentialState::Configured(alpaca()));
        assert!(!restarts_execution_stream(&classify(&disabled, false)));
    }

    /// The execution stream's registry key must never collide with a feed
    /// code — `restart_databento_feed` keys the same `StreamRegistry` by bare
    /// connection code ("databento-opra"), so an execution stream's key must
    /// be shaped so it can never equal one, for any environment.
    #[test]
    fn alpaca_exec_stream_code_cannot_collide_with_a_feed_code() {
        assert_eq!(alpaca_exec_stream_code("PAPER"), "alpaca-paper:exec");
        assert_eq!(alpaca_exec_stream_code("LIVE"), "alpaca-live:exec");
        assert_ne!(alpaca_exec_stream_code("PAPER"), "databento-opra");
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
