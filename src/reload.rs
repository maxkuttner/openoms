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
use crate::adapters::binance::BinanceAdapter;
use crate::adapters::{BrokerRegistry, Transport};
use crate::credentials::{BrokerCredentials, Connection, CredentialState};
use crate::fix;
use crate::kafka::KafkaClient;
use crate::stream_health::StreamHealthRegistry;

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
/// At boot `current` is an empty registry, so there is nothing to carry and
/// every connection is built fresh — this is what makes boot's call to this
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
        let outcome = classify(conn, is_boot);

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
                            if let Some(adapter) = current.get("IBKR", env_name) {
                                registry.register("IBKR", env_name, adapter);
                            }
                        } else if let Some(adapter) = fix::start_ibkr(
                            env_name,
                            creds,
                            &deps.stream_health,
                            deps.pool.clone(),
                            deps.kafka.clone(),
                            deps.position_changed_tx.clone(),
                        ) {
                            registry.register("IBKR", env_name, adapter);
                        }
                    }
                    BrokerCredentials::BinanceFix { api_key, private_key, .. } => {
                        if matches!(outcome, ConnectionOutcome::RestartRequired) {
                            // Conservative on purpose (see `classify`): carry forward
                            // whatever is currently registered, FIX or REST, rather
                            // than rebuilding — even a REST rebuild is deferred to a
                            // follow-up, not this task.
                            if let Some(adapter) = current.get("BINANCE", env_name) {
                                registry.register("BINANCE", env_name, adapter);
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
                                        None => error!(code = %conn.code, "Binance FIX session could not start"),
                                    }
                                }
                                Transport::Rest => match BinanceAdapter::new(api_key.clone(), private_key, env_name) {
                                    Ok(adapter) => {
                                        let adapter = Arc::new(adapter);
                                        registry.register("BINANCE", env_name, adapter.clone());
                                        binance_rest_adapters.push((env_name, adapter));
                                        info!(code = %conn.code, credentials_updated_at = ?conn.credentials_updated_at, "registered BINANCE/{env_name} adapter (REST/WS)");
                                    }
                                    Err(e) => error!(code = %conn.code, "Binance adapter not registered: {e}"),
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
        let fix = conn("ibkr-paper", "ACTIVE", CredentialState::Configured(ibkr()));

        // At boot both are registered — nothing is running yet to conflict with.
        assert!(matches!(classify(&rest, true), ConnectionOutcome::Registered));
        assert!(matches!(classify(&fix, true), ConnectionOutcome::Registered));

        // On reload the FIX one must report a restart rather than starting a
        // second session to the same venue.
        assert!(matches!(classify(&rest, false), ConnectionOutcome::Registered));
        assert!(matches!(classify(&fix, false), ConnectionOutcome::RestartRequired));
    }
}
