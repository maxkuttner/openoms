//! Reconciler tests: a `FakeSource`, a fake engine that answers each venue
//! event, and a freshly migrated database (`OMS_RELOAD_TEST_DATABASE_URL`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use gateway::adapter::{BrokerAdapter, BrokerError};
use gateway::contract::{BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState, EngineHandle, GatewayMsg, IngestOutcome};
use gateway::venue_event::{Cursor, ExecutionSource, PullHints, PullPage, VenueEvent, VenueFill, VenueOrderStatus, VenueOrderUpdate};
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use super::{spawn, spawn_with, ReconcilerHandle};
use crate::ingest::apply::tests::{world, World};

#[derive(Default)]
struct FakeSource {
    pages: Mutex<Vec<PullPage>>,
    /// `from` of each `fills_since` call.
    calls: Mutex<Vec<Option<String>>>,
    sinces: Mutex<Vec<DateTime<Utc>>>,
    scopes_calls: Mutex<usize>,
    /// When set, `scopes` waits for a permit (to hold a pull open).
    gate: Option<Semaphore>,
    max_history: Option<chrono::Duration>,
    /// Pull per symbol, with these as the venue's own scopes.
    venue_symbols: Option<Vec<String>>,
    /// What `held_assets` answers.
    held: Vec<String>,
    /// `held_assets` calls that fail before it answers.
    held_failures: Mutex<usize>,
    held_calls: Mutex<usize>,
    /// The scope of each `fills_since` call.
    pulled_scopes: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ExecutionSource for FakeSource {
    async fn fills_since(&self, _: &str, scope: &str, from: Option<&Cursor>, since: DateTime<Utc>) -> Result<PullPage, BrokerError> {
        self.pulled_scopes.lock().unwrap().push(scope.to_string());
        self.sinces.lock().unwrap().push(since);
        self.calls.lock().unwrap().push(from.map(|c| c.0.clone()));
        let mut pages = self.pages.lock().unwrap();
        Ok(if pages.is_empty() {
            PullPage { fills: vec![], next: Cursor(from.map(|c| c.0.clone()).unwrap_or_default()), more: false }
        } else {
            pages.remove(0)
        })
    }
    async fn open_order_snapshot(&self, _: &str) -> Result<Vec<VenueOrderUpdate>, BrokerError> {
        Ok(vec![])
    }
    async fn scopes(&self, _: &str) -> Result<Vec<String>, BrokerError> {
        *self.scopes_calls.lock().unwrap() += 1;
        if let Some(g) = &self.gate {
            g.acquire().await.unwrap().forget();
        }
        Ok(self.venue_symbols.clone().unwrap_or_else(|| vec![String::new()]))
    }
    async fn held_assets(&self, _: &str) -> Result<Vec<String>, BrokerError> {
        *self.held_calls.lock().unwrap() += 1;
        let mut failures = self.held_failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(BrokerError::Network("418 I'm a teapot".into()));
        }
        Ok(self.held.clone())
    }
    fn hints(&self) -> PullHints {
        PullHints { intraday_secs: 3600, max_history: self.max_history, per_symbol: self.venue_symbols.is_some() }
    }
}

/// Reports the order canceled with 2 executed.
struct ClosedAtVenue;

#[async_trait::async_trait]
impl BrokerAdapter for ClosedAtVenue {
    async fn submit_order(&self, _: &gateway::adapter::BrokerOrderRequest) -> Result<gateway::adapter::BrokerOrderResponse, BrokerError> {
        unreachable!()
    }
    async fn cancel_order(&self, _: &str, _: &str) -> Result<(), BrokerError> {
        unreachable!()
    }
    async fn open_orders(&self) -> Result<Vec<BrokerOpenOrder>, BrokerError> {
        Ok(vec![])
    }
    async fn order_status(&self, _: &str, _: &str, _: &str) -> Result<BrokerOrderState, BrokerError> {
        Ok(BrokerOrderState { outcome: BrokerOrderOutcome::Canceled, executed_qty: 2.0, avg_px: 100.0 })
    }
}

type Seen = Arc<Mutex<Vec<VenueEvent>>>;

/// An engine that records each venue event and answers it with `answer`.
fn fake_engine(answer: impl Fn(&VenueEvent) -> Result<IngestOutcome, String> + Send + 'static) -> (EngineHandle, Seen) {
    let (engine, mut rx) = EngineHandle::new();
    let seen: Seen = Arc::default();
    let s = seen.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let GatewayMsg::Venue { event, reply, .. } = msg {
                let out = answer(&event);
                s.lock().unwrap().push(event);
                if let Some(r) = reply {
                    let _ = r.send(out);
                }
            }
        }
    });
    (engine, seen)
}

fn ok(_: &VenueEvent) -> Result<IngestOutcome, String> {
    Ok(IngestOutcome::default())
}

fn fill(exec: &str) -> VenueFill {
    VenueFill {
        exec_id: exec.into(),
        venue_order_id: "v1".into(),
        client_order_id: None,
        account_ref: "acc".into(),
        symbol: "BTCUSDT".into(),
        buy: true,
        qty: 1.0,
        price: 100.0,
        fee: None,
        executed_at: None,
    }
}

fn page(execs: &[&str], next: &str, more: bool) -> PullPage {
    PullPage { fills: execs.iter().map(|e| fill(e)).collect(), next: Cursor(next.into()), more }
}

fn start(w: &World, source: Arc<FakeSource>, engine: EngineHandle) -> (ReconcilerHandle, mpsc::Sender<VenueEvent>) {
    let (tx, rx) = mpsc::channel(16);
    (spawn(w.conn.clone(), source, rx, engine, w.pool.clone()), tx)
}

async fn set_cursor(w: &World, cursor: &str) {
    sqlx::query("INSERT INTO recon_cursor (broker_connection_code, account_id, scope, cursor) VALUES ($1, $2, '', $3)")
        .bind(&w.conn).bind(w.account).bind(cursor).execute(&w.pool).await.unwrap();
}

async fn cursor(w: &World) -> Option<String> {
    sqlx::query_scalar("SELECT cursor FROM recon_cursor WHERE broker_connection_code = $1 AND account_id = $2")
        .bind(&w.conn).bind(w.account).fetch_optional(&w.pool).await.unwrap()
}

async fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

fn fill_ids(seen: &Seen) -> Vec<String> {
    seen.lock().unwrap().iter().filter_map(|e| if let VenueEvent::Fill(f) = e { Some(f.exec_id.clone()) } else { None }).collect()
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn new_account_starts_cursor_at_now_without_history() {
    let w = world().await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["old"], "c1", false)]), ..Default::default() });
    let (engine, seen) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    for _ in 0..500 {
        if cursor(&w).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(cursor(&w).await.as_deref(), Some(""));
    assert!(source.calls.lock().unwrap().is_empty(), "no history pulled");
    assert!(fill_ids(&seen).is_empty());
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn pages_until_more_is_false_and_saves_last_cursor() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let pages = vec![page(&["e1", "e2"], "c1", true), page(&["e3"], "c2", false)];
    let source = Arc::new(FakeSource { pages: Mutex::new(pages), ..Default::default() });
    let (engine, seen) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| fill_ids(&seen).len() == 3).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(cursor(&w).await.as_deref(), Some("c2"));
    assert_eq!(fill_ids(&seen), ["e1", "e2", "e3"]);
    assert_eq!(*source.calls.lock().unwrap(), [Some("c0".to_string()), Some("c1".to_string())]);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn cursor_not_advanced_when_ingest_fails() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let pages = vec![page(&["e1"], "c1", true), page(&["e2", "e3"], "c2", false)];
    let source = Arc::new(FakeSource { pages: Mutex::new(pages), ..Default::default() });
    let (engine, seen) = fake_engine(|e| match e {
        VenueEvent::Fill(f) if f.exec_id == "e2" => Err("db down".into()),
        _ => Ok(IngestOutcome::default()),
    });
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| fill_ids(&seen).contains(&"e2".to_string())).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(cursor(&w).await.as_deref(), Some("c1"));
    assert!(!fill_ids(&seen).contains(&"e3".to_string()), "page abandoned at the failure");
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn triggers_while_busy_coalesce_into_one_rerun() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let source = Arc::new(FakeSource { gate: Some(Semaphore::new(0)), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| *source.scopes_calls.lock().unwrap() == 1).await;
    h.pull_now();
    h.pull_now();
    h.pull_now();
    source.gate.as_ref().unwrap().add_permits(10);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*source.scopes_calls.lock().unwrap(), 2);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn working_order_missing_from_snapshot_is_resolved() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM principal").fetch_one(&w.pool).await.unwrap();
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, limit_price, original_qty, \
         leaves_qty, cum_qty, status, version, origin, external_order_id) \
         VALUES ($1, 'mine', $2, $3, $4, $5, $6, 'buy', 'limit', 'gtc', 100, 5, 5, 0, 'routed', 0, 'API', 'v-oms')",
    )
    .bind(order_id).bind(principal).bind(w.portfolio).bind(w.account).bind(&w.conn).bind(w.instrument.to_string())
    .execute(&w.pool).await.unwrap();
    let (engine, seen) = fake_engine(ok);
    let (_tx, rx) = mpsc::channel(16);
    let h = spawn_with(w.conn.clone(), Arc::new(FakeSource::default()), rx, engine, w.pool.clone(), Some(Arc::new(ClosedAtVenue)), None);
    h.pull_now();
    eventually(|| seen.lock().unwrap().iter().any(|e| matches!(e, VenueEvent::Order(_)))).await;
    let events = seen.lock().unwrap().clone();
    let [VenueEvent::Order(o)] = events.as_slice() else { panic!("one order update, got {events:?}") };
    assert_eq!(o.client_order_id.as_deref(), Some(order_id.to_string().as_str()));
    assert_eq!(o.venue_order_id, "v-oms");
    assert_eq!(o.symbol, "BTCUSDT");
    assert_eq!(o.status, VenueOrderStatus::Canceled);
    assert_eq!((o.qty, o.cum_qty, o.buy), (5.0, 2.0, true));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn empty_next_cursor_does_not_slide_since() {
    let w = world().await;
    set_cursor(&w, "").await;
    let stamp = |w: &World| {
        let pool = w.pool.clone();
        async move { sqlx::query_scalar::<_, DateTime<Utc>>("SELECT updated_at FROM recon_cursor").fetch_one(&pool).await.unwrap() }
    };
    let before = stamp(&w).await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["e1"], "", false), page(&[], "", false)]), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| source.sinces.lock().unwrap().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.pull_now();
    eventually(|| source.sinces.lock().unwrap().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stamp(&w).await, before);
    assert_eq!(cursor(&w).await.as_deref(), Some(""));
    let want = before - chrono::Duration::seconds(60);
    assert_eq!(*source.sinces.lock().unwrap(), [want, want]);
}

fn dup(_: &VenueEvent) -> Result<IngestOutcome, String> {
    Ok(IngestOutcome { duplicate: true, ..Default::default() })
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_clamps_to_venue_max_history() {
    let w = world().await;
    let source = Arc::new(FakeSource { max_history: Some(chrono::Duration::days(7)), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    let now = Utc::now();
    let r = h.sweep_at(now, chrono::Duration::days(30)).await;
    assert_eq!(r.requested_from, now - chrono::Duration::days(30));
    assert_eq!(r.obtained_from, now - chrono::Duration::days(7));
    assert_eq!(*source.sinces.lock().unwrap(), [now - chrono::Duration::days(7)]);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_counts_duplicates_not_rebooks() {
    let w = world().await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["e1"], "", false)]), ..Default::default() });
    let (engine, _) = fake_engine(dup);
    let (h, _tx) = start(&w, source, engine);
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!((r.booked, r.duplicates, r.breaks), (0, 1, 0));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_counts_booked_and_breaks_and_leaves_cursors_alone() {
    let w = world().await;
    set_cursor(&w, "keep").await;
    let source = Arc::new(FakeSource {
        pages: Mutex::new(vec![page(&["e1", "e2", "e3"], "p2", true), page(&["e4"], "p3", false)]),
        ..Default::default()
    });
    let (engine, _) = fake_engine(|e| match e {
        VenueEvent::Fill(f) if f.exec_id == "e2" => Ok(IngestOutcome { broken: true, ..Default::default() }),
        VenueEvent::Fill(f) if f.exec_id == "e3" => Ok(IngestOutcome { duplicate: true, ..Default::default() }),
        _ => Ok(IngestOutcome::default()),
    });
    let (h, _tx) = start(&w, source.clone(), engine);
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!((r.booked, r.duplicates, r.breaks, r.failed), (2, 1, 1, 0));
    assert_eq!(*source.calls.lock().unwrap(), [None, Some("p2".to_string())]);
    assert_eq!(cursor(&w).await.as_deref(), Some("keep"));
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_stops_on_more_with_empty_next() {
    let w = world().await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["e1"], "", true), page(&["e2"], "", true)]), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!(r.booked, 1);
    assert_eq!(source.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_intraday_pull_stops_on_more_with_empty_next() {
    let w = world().await;
    set_cursor(&w, "c").await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["e1"], "", true), page(&["e2"], "", true)]), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| !source.calls.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(source.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_registry_does_not_keep_a_reconciler_alive() {
    let w = world().await;
    let (engine, _) = fake_engine(ok);
    let (_tx, rx) = mpsc::channel(1);
    // Its own connection name: not the world's, to show the registry by name.
    let h = spawn("registry-test".into(), Arc::new(FakeSource::default()), rx, engine, w.pool.clone());
    assert!(super::live("registry-test").is_some());
    drop(h);
    assert!(super::live("registry-test").is_none());
}

/// Everything after an established cursor is new, even a late-indexed trade
/// stamped before the cursor's time: no `since` filter on any page.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn cursor_resume_pull_passes_no_since_filter() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let pages = vec![page(&["e1"], "c1", true), page(&["e2"], "c2", false)];
    let source = Arc::new(FakeSource { pages: Mutex::new(pages), ..Default::default() });
    let (engine, seen) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| fill_ids(&seen).len() == 2).await;
    assert_eq!(*source.sinces.lock().unwrap(), [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MIN_UTC]);
}

/// A per-symbol venue only knows its open orders' symbols; the OMS adds the
/// symbols it has orders on for the account, so a filled order's symbol is pulled.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn per_symbol_scopes_add_the_accounts_order_symbols() {
    let w = world().await;
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM principal").fetch_one(&w.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
         broker_connection_code, instrument_id, side, order_type, time_in_force, limit_price, original_qty, \
         leaves_qty, cum_qty, status, version, origin) \
         VALUES ($1, 'mine', $2, $3, $4, $5, $6, 'buy', 'limit', 'gtc', 100, 5, 0, 5, 'filled', 0, 'API')",
    )
    .bind(Uuid::new_v4()).bind(principal).bind(w.portfolio).bind(w.account).bind(&w.conn).bind(w.instrument.to_string())
    .execute(&w.pool).await.unwrap();
    let source = Arc::new(FakeSource { venue_symbols: Some(vec!["ETHUSDT".into()]), ..Default::default() });
    let (engine, _seen) = fake_engine(ok);
    let (h, _tx) = start(&w, source, engine);
    h.pull_now();
    let scopes = || async {
        sqlx::query_scalar::<_, String>("SELECT scope FROM recon_cursor WHERE account_id = $1 ORDER BY scope")
            .bind(w.account).fetch_all(&w.pool).await.unwrap()
    };
    for _ in 0..500 {
        if scopes().await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(scopes().await, ["BTCUSDT", "ETHUSDT"]);
}

/// The accounts with a recon cursor on the world's connection, main account last.
async fn cursor_accounts(w: &World) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT account_id FROM recon_cursor WHERE broker_connection_code = $1 ORDER BY account_id = $2")
        .bind(&w.conn).bind(w.account).fetch_all(&w.pool).await.unwrap()
}

/// Portfolios sharing a venue grant have one account row each with the same
/// ref: the reconciler pulls that ref once.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn reconciler_pulls_shared_ref_once() {
    let w = world().await;
    crate::ingest::apply::tests::shared_ref_account(&w, "P2", "ACTIVE").await;
    crate::ingest::apply::tests::shared_ref_account(&w, "P3", "CLOSED").await;
    sqlx::query("UPDATE account SET status = 'CLOSED' WHERE id = $1").bind(w.account).execute(&w.pool).await.unwrap();
    let (engine, _) = fake_engine(ok);
    let (main, _tx) = start(&w, Arc::new(FakeSource::default()), engine);
    main.pull_now();
    for _ in 0..500 {
        if !cursor_accounts(&w).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(cursor_accounts(&w).await, [w.account], "one pull for the shared ref, on the oldest account, even closed");
    drop(main);
}

/// A pair traded entirely during an outage (no open order, no OMS order) is
/// found from the account's balance of its base asset.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn sweep_discovers_symbol_from_nonzero_balance() {
    let w = world().await;
    let source = Arc::new(FakeSource { venue_symbols: Some(vec![]), held: vec!["BTC".into()], ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!(*source.pulled_scopes.lock().unwrap(), ["BTCUSDT"]);

    // Without the balance, nothing points at the pair.
    let blind = Arc::new(FakeSource { venue_symbols: Some(vec![]), ..Default::default() });
    let (engine, _) = fake_engine(ok);
    let (h, _tx) = start(&w, blind.clone(), engine);
    h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert!(blind.pulled_scopes.lock().unwrap().is_empty());
}

/// A failed balance read must not cost the known scopes, and discovery is
/// retried on the next pull instead of waiting for the EOD sweep.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn held_assets_failure_keeps_existing_scopes_and_retries_discovery() {
    let w = world().await;
    sqlx::query("INSERT INTO recon_cursor (broker_connection_code, account_id, scope, cursor) VALUES ($1, $2, 'ETHUSDT', 'c0'), ($1, $2, 'BTCUSDT', 'c0')")
        .bind(&w.conn).bind(w.account).execute(&w.pool).await.unwrap();
    let source = Arc::new(FakeSource {
        venue_symbols: Some(vec!["ETHUSDT".into()]),
        held: vec!["BTC".into()],
        held_failures: Mutex::new(1),
        ..Default::default()
    });
    let (engine, _) = fake_engine(ok);
    let (h, tx) = start(&w, source.clone(), engine);
    tx.send(VenueEvent::Connected).await.unwrap();
    eventually(|| *source.held_calls.lock().unwrap() == 1 && !source.pulled_scopes.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(*source.pulled_scopes.lock().unwrap(), ["ETHUSDT"], "the known scope is pulled despite the failure");

    h.pull_now();
    eventually(|| source.pulled_scopes.lock().unwrap().contains(&"BTCUSDT".to_string())).await;
    assert_eq!(*source.held_calls.lock().unwrap(), 2, "discovery retried on the next pull");

    // EOD sweep: same, the account is not skipped.
    *source.held_failures.lock().unwrap() = 1;
    source.pulled_scopes.lock().unwrap().clear();
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert!(r.errors.is_empty());
    assert_eq!(*source.pulled_scopes.lock().unwrap(), ["ETHUSDT"]);
}

/// A pair found on reconnect (balance discovery) on an account that already
/// has cursors is pulled at once from the account's oldest cursor stamp, not
/// started at now (which would leave the outage's fills to the EOD sweep).
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn reconnect_discovered_symbol_pulls_since_account_watermark() {
    let w = world().await;
    let stamp = Utc::now() - chrono::Duration::hours(3);
    sqlx::query("INSERT INTO recon_cursor (broker_connection_code, account_id, scope, cursor, updated_at) VALUES ($1, $2, 'ETHUSDT', 'c0', $3)")
        .bind(&w.conn).bind(w.account).bind(stamp).execute(&w.pool).await.unwrap();
    let source = Arc::new(FakeSource {
        venue_symbols: Some(vec!["ETHUSDT".into()]),
        held: vec!["BTC".into()],
        ..Default::default()
    });
    let (engine, _) = fake_engine(ok);
    let (_h, tx) = start(&w, source.clone(), engine);
    tx.send(VenueEvent::Connected).await.unwrap();
    eventually(|| source.pulled_scopes.lock().unwrap().contains(&"BTCUSDT".to_string())).await;
    let scopes = source.pulled_scopes.lock().unwrap().clone();
    let i = scopes.iter().position(|s| s == "BTCUSDT").unwrap();
    assert_eq!(source.calls.lock().unwrap()[i], None, "no cursor yet");
    assert_eq!(
        source.sinces.lock().unwrap()[i].timestamp_micros(),
        (stamp - super::SINCE_MARGIN).timestamp_micros()
    );
    let saved: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM recon_cursor WHERE broker_connection_code = $1 AND account_id = $2 AND scope = 'BTCUSDT'")
        .bind(&w.conn).bind(w.account).fetch_one(&w.pool).await.unwrap();
    assert_eq!(saved.timestamp_micros(), stamp.timestamp_micros());
}

/// Pre-upgrade fills on a connection with a `recon_not_before` watermark are
/// not re-read by pulls or sweeps (their legacy ids could not be re-keyed).
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn watermark_blocks_pre_upgrade_refetch() {
    let w = world().await;
    let upgrade = Utc::now() - chrono::Duration::hours(1);
    sqlx::query("UPDATE broker_connection SET recon_not_before = $2 WHERE code = $1")
        .bind(&w.conn).bind(upgrade).execute(&w.pool).await.unwrap();
    let at = |f: VenueFill, t: DateTime<Utc>| VenueFill { executed_at: Some(t), ..f };
    let fills = || vec![at(fill("old"), upgrade - chrono::Duration::minutes(5)), at(fill("new"), upgrade + chrono::Duration::minutes(5))];
    set_cursor(&w, "c0").await;
    let source = Arc::new(FakeSource {
        pages: Mutex::new(vec![
            PullPage { fills: fills(), next: Cursor("c1".into()), more: false },
            PullPage { fills: fills(), next: Cursor(String::new()), more: false },
        ]),
        ..Default::default()
    });
    let (engine, seen) = fake_engine(ok);
    let (h, _tx) = start(&w, source.clone(), engine);
    h.pull_now();
    eventually(|| !fill_ids(&seen).is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fill_ids(&seen), ["new"]);
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!((r.booked, r.failed), (1, 0));
    assert_eq!(fill_ids(&seen), ["new", "new"], "the sweep drops the pre-upgrade fill too");
}

/// A fill that fails ingest on every pull becomes an `ingest_error` break on its
/// third failure and the scope moves past it.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn persistently_failing_fill_becomes_ingest_error_break_and_scope_advances() {
    let w = world().await;
    set_cursor(&w, "c0").await;
    let pages = (0..3).map(|_| page(&["bad", "good"], "c1", false)).collect();
    let source = Arc::new(FakeSource { pages: Mutex::new(pages), ..Default::default() });
    let (engine, seen) = fake_engine(|e| match e {
        VenueEvent::Fill(f) if f.exec_id == "bad" => Err("poisoned".into()),
        _ => Ok(IngestOutcome::default()),
    });
    let (h, _tx) = start(&w, source.clone(), engine);
    for n in 1..=3 {
        h.pull_now();
        eventually(|| fill_ids(&seen).iter().filter(|e| *e == "bad").count() == n).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        if n < 3 {
            assert_eq!(cursor(&w).await.as_deref(), Some("c0"), "pull {n}: wedged on the failure");
        }
    }
    eventually(|| fill_ids(&seen).contains(&"good".to_string())).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(cursor(&w).await.as_deref(), Some("c1"));
    let (kind, events): (String, serde_json::Value) = sqlx::query_as(
        "SELECT kind, events_json FROM recon_break WHERE broker_connection_code = $1 AND account_id = $2 AND venue_order_id = 'v1' AND status = 'open'",
    )
    .bind(&w.conn).bind(w.account).fetch_one(&w.pool).await.unwrap();
    assert_eq!(kind, "ingest_error");
    assert_eq!((events[0]["exec_id"].as_str(), events[0]["error"].as_str()), (Some("bad"), Some("poisoned")));
}

/// A sweep keeps the text of each failed ingest in its report.
#[tokio::test]
#[ignore = "needs OMS_RELOAD_TEST_DATABASE_URL"]
async fn eod_sweep_reports_failed_ingest_errors() {
    let w = world().await;
    let source = Arc::new(FakeSource { pages: Mutex::new(vec![page(&["e1"], "", false)]), ..Default::default() });
    let (engine, _) = fake_engine(|_| Err("db down".into()));
    let (h, _tx) = start(&w, source, engine);
    let r = h.sweep_at(Utc::now(), chrono::Duration::days(1)).await;
    assert_eq!((r.booked, r.failed), (0, 1));
    assert_eq!(r.errors.len(), 1);
    assert!(r.errors[0].contains("e1") && r.errors[0].contains("db down"), "{:?}", r.errors);
    assert!(crate::jobs::eod_sweep::failure(&r).is_some());
}
