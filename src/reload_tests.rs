//! HTTP/store orchestration tests. Only external broker/feed sessions are fake.
//! The ignored test creates its own database using an explicit disposable-server
//! URL, migrates the real schema, and never loads oms.toml or .env.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, middleware, routing::post, Router};
use sqlx::{postgres::PgConnectOptions, PgPool};
use tokio::sync::{mpsc, Notify};

use crate::adapters::{alpaca::AlpacaAdapter, BrokerRegistry};
use crate::app_state::{AppState, StreamRegistry};
use crate::credentials::{self, BrokerCredentials, FeedCredentials};
use crate::reload::{self, RegistrationDeps, ReloadStreams};
use crate::secrets::{parse_master_key, MasterKey};
use crate::stream_health::{StreamHealthRegistry, StreamKind, StreamState};

fn key() -> MasterKey {
    parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
}

fn adapter() -> Arc<AlpacaAdapter> {
    Arc::new(AlpacaAdapter::new("synthetic-key".into(), "synthetic-secret".into(), "PAPER"))
}

fn state(pool: PgPool) -> AppState {
    let (quote_tx, _) = mpsc::channel(1);
    AppState::new(pool, "test-admin".into(), true, BrokerRegistry::new(), None,
        symbology::Identifier::new(symbology::OpenFigiClient::new(None), symbology::InMemoryCache::new()),
        StreamHealthRegistry::new(), None, quote_tx)
}

#[derive(Clone, Default)]
struct FakeStreams {
    active: Arc<Mutex<HashMap<String, usize>>>,
    adapters: Arc<Mutex<HashMap<String, Arc<AlpacaAdapter>>>>,
    gate: Arc<Mutex<Option<StartGate>>>,
}

struct StartGate {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

struct Running {
    code: String,
    active: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        *self.active.lock().unwrap().get_mut(&self.code).unwrap() -= 1;
    }
}

impl FakeStreams {
    fn spawn(&self, code: &str, streams: &StreamRegistry) {
        let mut active = self.active.lock().unwrap();
        let count = active.entry(code.into()).or_default();
        assert_eq!(*count, 0, "replacement started before old task exited: {code}");
        *count += 1;
        drop(active);
        let running = Running { code: code.into(), active: self.active.clone() };
        streams.insert(code, tokio::spawn(async move {
            let _running = running;
            std::future::pending::<()>().await;
        }));
    }

    fn count(&self, code: &str) -> usize {
        self.active.lock().unwrap().get(code).copied().unwrap_or(0)
    }

    fn pause_next_start(&self) -> (Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(StartGate { entered: entered.clone(), release: release.clone() });
        (entered, release)
    }
}

#[async_trait::async_trait]
impl ReloadStreams for FakeStreams {
    async fn alpaca(&self, environment: &'static str, _: (String, String),
        adapter: Arc<AlpacaAdapter>, _: &RegistrationDeps, streams: &StreamRegistry) {
        let gate = self.gate.lock().unwrap().take();
        if let Some(StartGate { entered, release }) = gate {
            entered.notify_one();
            release.notified().await;
        }
        let code = reload::alpaca_exec_stream_code(environment);
        streams.abort_and_remove(&code).await;
        self.spawn(&code, streams);
        self.adapters.lock().unwrap().insert(environment.into(), adapter);
    }

    async fn databento(&self, _: String, state: &AppState) {
        state.streams().abort_and_remove("databento-opra").await;
        self.spawn("databento-opra", state.streams());
        let (tx, mut rx) = mpsc::channel(1);
        state.doorbells().set("databento-opra", tx);
        state.doorbells().ring_all();
        assert!(rx.try_recv().is_ok(), "replacement feed must receive doorbells");
    }
}

async fn serve(state: AppState, fake: FakeStreams) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/admin/connections/reload", post(
        move |State(state): State<AppState>| {
            let fake = fake.clone();
            async move { crate::admin::reload_using(state, Some(key()), fake).await }
        }))
        .route_layer(middleware::from_fn_with_state(state.clone(), crate::auth::admin_middleware))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/admin/connections/reload", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    (url, handle)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap()
}

async fn post_reload(url: &str) -> (u16, serde_json::Value) {
    let response = client().post(url).bearer_auth("test-admin").send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
}

#[tokio::test]
async fn reload_http_auth_and_store_failure_preserve_state() {
    // A closed lazy pool fails without ever opening a network connection.
    let pool = PgPool::connect_lazy("postgres://localhost/never_connect").unwrap();
    pool.close().await;
    let state = state(pool);
    let before = adapter();
    let mut registry = BrokerRegistry::new();
    registry.register_alpaca("PAPER", before.clone());
    state.swap_registry(registry);
    let fake = FakeStreams::default();
    fake.spawn("alpaca-paper:exec", state.streams());
    let (url, server) = serve(state.clone(), fake.clone()).await;

    assert_eq!(client().post(&url).send().await.unwrap().status(), 403);
    assert_eq!(client().post(&url).bearer_auth("wrong").send().await.unwrap().status(), 403);
    assert_eq!(post_reload(&url).await.0, 500);
    assert!(Arc::ptr_eq(&before, &state.registry().get_alpaca("PAPER").unwrap()));
    assert_eq!(fake.count("alpaca-paper:exec"), 1);

    state.streams().abort_and_remove("alpaca-paper:exec").await;
    server.abort();
    let _ = server.await;
}

fn outcome<'a>(body: &'a serde_json::Value, code: &str) -> &'a serde_json::Value {
    &body["connections"].as_array().unwrap().iter()
        .find(|row| row[0] == code).unwrap()[1]
}

async fn insert_broker(pool: &PgPool, code: &str, kind: &str) {
    sqlx::query("INSERT INTO oms.broker_connection (code, broker_code, environment) VALUES ($1, $2, 'PAPER')")
        .bind(code).bind(kind).execute(pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn reload_http_lifecycle_against_postgres() {
    let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL")
        .expect("set OMS_RELOAD_TEST_DATABASE_URL to an explicit disposable Postgres server");
    let options: PgConnectOptions = url.parse().unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let name = format!("oms_reload_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name} OWNER oms")).execute(&admin).await.unwrap();
    let pool = PgPool::connect_with(options.database(&name)).await.unwrap();
    crate::setup::database::migrate::ensure_tracking(&pool).await.unwrap();
    crate::setup::database::migrate::apply_all(&pool).await.unwrap();

    let state = state(pool.clone());
    let fake = FakeStreams::default();
    let (url, server) = serve(state.clone(), fake.clone()).await;
    assert_eq!(post_reload(&url).await, (200, serde_json::json!({"connections": []})));

    insert_broker(&pool, "alpaca-paper", "ALPACA").await;
    insert_broker(&pool, "ibkr-paper", "IBKR").await;
    insert_broker(&pool, "binance-paper", "BINANCE").await;
    credentials::save_broker(&pool, &key(), "alpaca-paper", &BrokerCredentials::Alpaca {
        key: "SYNTHETIC-API-KEY".into(), secret: "SYNTHETIC-SECRET".into(),
    }).await.unwrap();
    credentials::save_broker(&pool, &key(), "ibkr-paper", &BrokerCredentials::IbkrFix {
        host: "never-connect.invalid".into(), port: 1, sender_comp_id: "OMS".into(),
        target_comp_id: "IBKR".into(), password: "SYNTHETIC-FIX-PASSWORD".into(), ssl: true,
    }).await.unwrap();
    credentials::save_broker(&pool, &key(), "binance-paper", &BrokerCredentials::BinanceFix {
        host: "never-connect.invalid".into(), port: 1, sender_comp_id: "OMS".into(),
        target_comp_id: "SPOT".into(), api_key: "SYNTHETIC-BINANCE".into(), private_key: "SYNTHETIC-PEM".into(),
    }).await.unwrap();
    credentials::save_feed(&pool, &key(), "databento-opra", &FeedCredentials::Databento {
        api_key: "SYNTHETIC-DATABENTO".into(),
    }).await.unwrap();

    let mut registry = BrokerRegistry::new();
    registry.register("IBKR", "PAPER", adapter());
    registry.register("BINANCE", "PAPER", adapter());
    state.swap_registry(registry);
    let held_fix = state.registry().get("IBKR", "PAPER").unwrap();
    fake.spawn("binance-paper:exec", state.streams());
    for _ in 0..3 {
        let (status, body) = post_reload(&url).await;
        assert_eq!(status, 200);
        assert_eq!(outcome(&body, "alpaca-paper"), "Registered");
        assert_eq!(outcome(&body, "databento-opra"), "Registered");
        assert_eq!(outcome(&body, "ibkr-paper"), "RestartRequired");
        assert_eq!(outcome(&body, "binance-paper"), "RestartRequired");
        assert!(!body.to_string().contains("SYNTHETIC"));
        assert!(Arc::ptr_eq(&held_fix, &state.registry().get("IBKR", "PAPER").unwrap()));
        assert!(Arc::ptr_eq(&fake.adapters.lock().unwrap()["PAPER"], &state.registry().get_alpaca("PAPER").unwrap()));
        assert_eq!(fake.count("alpaca-paper:exec"), 1);
        assert_eq!(fake.count("databento-opra"), 1);
    }

    // Pause after publication, exactly where another reload used to interleave.
    let (entered, release) = fake.pause_next_start();
    let first = tokio::spawn({
        let state = state.clone(); let fake = fake.clone();
        async move { crate::admin::reload_using(state, Some(key()), fake).await }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified()).await.unwrap();
    let during = state.registry().get_alpaca("PAPER").unwrap();
    let mut second = tokio::spawn({ let url = url.clone(); async move { post_reload(&url).await } });
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut second).await.is_err());
    assert!(Arc::ptr_eq(&during, &state.registry().get_alpaca("PAPER").unwrap()));
    // Cancel the caller. The owned reload must still finish and release its lock.
    first.abort();
    let _ = first.await;
    release.notify_one();
    assert_eq!(second.await.unwrap().0, 200);
    assert_eq!(fake.count("alpaca-paper:exec"), 1);
    assert!(Arc::ptr_eq(&fake.adapters.lock().unwrap()["PAPER"], &state.registry().get_alpaca("PAPER").unwrap()));

    // Partial unreadability preserves the exact old adapter and feed task.
    let before = state.registry().get_alpaca("PAPER").unwrap();
    sqlx::query("UPDATE oms.broker_connection SET credentials = '\\x00'::bytea WHERE code = 'alpaca-paper'")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE oms.feed_connection SET credentials = '\\x00'::bytea")
        .execute(&pool).await.unwrap();
    let (status, body) = post_reload(&url).await;
    assert_eq!(status, 200);
    assert!(outcome(&body, "alpaca-paper")["Failed"].is_string());
    assert!(outcome(&body, "databento-opra")["Failed"].is_string());
    assert!(Arc::ptr_eq(&before, &state.registry().get_alpaca("PAPER").unwrap()));
    assert_eq!(fake.count("databento-opra"), 1);

    // No readable rows: refuse before swapping or stopping anything.
    sqlx::query("UPDATE oms.broker_connection SET credentials = '\\x00'::bytea")
        .execute(&pool).await.unwrap();
    assert_eq!(post_reload(&url).await.0, 500);
    assert!(Arc::ptr_eq(&before, &state.registry().get_alpaca("PAPER").unwrap()));
    assert_eq!(fake.count("alpaca-paper:exec"), 1);
    assert_eq!(fake.count("binance-paper:exec"), 1);
    assert_eq!(fake.count("databento-opra"), 1);

    // Deliberate removal is distinct from unreadability. Clear all blobs so
    // the whole-store refusal does not hide the disable/clear behavior.
    sqlx::query("UPDATE oms.broker_connection SET credentials = NULL, status = 'DISABLED'")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE oms.feed_connection SET credentials = NULL")
        .execute(&pool).await.unwrap();
    state.stream_health().handle("ALPACA", "PAPER", StreamKind::Execution).set_live();
    state.stream_health().handle("BINANCE", "PAPER", StreamKind::Execution).set_live();
    state.stream_health().handle("DATABENTO", "OPRA", StreamKind::Feed).set_live();
    let (status, body) = post_reload(&url).await;
    assert_eq!(status, 200);
    assert_eq!(outcome(&body, "alpaca-paper"), "Disabled");
    assert_eq!(outcome(&body, "databento-opra"), "Unconfigured");
    assert!(state.registry().get_alpaca("PAPER").is_none());
    assert!(state.registry().get("IBKR", "PAPER").is_none());
    for code in ["alpaca-paper:exec", "binance-paper:exec", "databento-opra"] {
        assert_eq!(fake.count(code), 0);
    }
    assert!(state.stream_health().snapshot().iter().all(|h| h.state == StreamState::Down));

    // Deleted rows have no report iteration, but their old tasks still stop.
    for code in ["alpaca-paper:exec", "binance-paper:exec", "databento-opra"] {
        fake.spawn(code, state.streams());
    }
    sqlx::query("DELETE FROM oms.broker_connection").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM oms.feed_connection").execute(&pool).await.unwrap();
    assert_eq!(post_reload(&url).await, (200, serde_json::json!({"connections": []})));
    for code in ["alpaca-paper:exec", "binance-paper:exec", "databento-opra"] {
        assert_eq!(fake.count(code), 0);
    }

    server.abort();
    let _ = server.await;
    pool.close().await;
    // SQLx may still have a connection finishing protocol shutdown. This name
    // belongs exclusively to this test, so terminate those sessions as well.
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)")).execute(&admin).await.unwrap();
    admin.close().await;
}
