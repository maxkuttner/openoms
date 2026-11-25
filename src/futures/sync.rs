//! Daily sync of listed futures contracts: for each active `future_root`, ask
//! IBKR for every listed contract and upsert it into the catalog.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use sqlx::PgPool;

use gateway::connectors::ibkr_tws::client::{ContractQuery, SecType, TwsClient, TwsContract, TwsError};
use gateway::connectors::ibkr_tws::resolve::{to_broker_instrument, RootSpec};
use gateway::connectors::ibkr_tws::{connection_code, live_client};

use crate::admin::AdminError;
use crate::app_state::AppState;
use crate::instruments_import::{live, save_instruments, EnvParam};
use crate::setup::brokers::Broker;

/// Delay before the first sync after boot, so the TWS session can come up.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// What one sync did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct SyncReport {
    /// Active roots asked about.
    pub roots: usize,
    /// Contracts saved.
    pub contracts: usize,
    /// Contracts IBKR listed that could not be mapped, with the reason.
    pub skipped: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct Root {
    code: String,
    asset_class: String,
    broker_symbol: String,
    broker_exchange: String,
    currency: String,
}

/// Pulls every listed contract of each active `future_root` from IBKR and saves
/// them. Contracts whose last trade day (America/Chicago) is past are dropped; one
/// that cannot be mapped is reported in `skipped` and the rest still save.
pub async fn sync_roots(pool: &PgPool, client: &dyn TwsClient) -> Result<SyncReport, String> {
    let roots: Vec<Root> = sqlx::query_as(
        "SELECT code, asset_class, broker_symbol, broker_exchange, currency \
         FROM future_root WHERE status = 'ACTIVE' ORDER BY code",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    // Postgres owns the tz database (see expiry.rs).
    let today: chrono::NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE 'America/Chicago')::date")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;

    let mut report = SyncReport { roots: roots.len(), ..Default::default() };
    let mut instruments = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in &roots {
        let q = ContractQuery {
            symbol: Some(root.broker_symbol.clone()),
            exchange: Some(root.broker_exchange.clone()),
            currency: Some(root.currency.clone()),
            sec_type: Some(SecType::Fut),
            ..Default::default()
        };
        let listed = client.contract_details(&q).await.map_err(|e: TwsError| format!("{}: {e}", root.code))?;
        let spec = RootSpec { code: root.code.clone(), asset_class: root.asset_class.clone() };
        for c in listed.iter().filter(|c| live_contract(c, today) && seen.insert(c.con_id)) {
            match to_broker_instrument(c, Some(&spec)) {
                Ok(i) => instruments.push(i),
                Err(e) => report.skipped.push(format!("{}: {e}", c.local_symbol)),
            }
        }
    }
    report.contracts = instruments.len();
    if !instruments.is_empty() {
        save_instruments(pool, instruments).await.map_err(|e| e.to_string())?;
    }
    Ok(report)
}

/// A contract with no known last trade day is kept; mapping reports it.
fn live_contract(c: &TwsContract, today: chrono::NaiveDate) -> bool {
    c.last_trade.is_none_or(|d| d >= today)
}

/// Daily loop: first run 60 s after start, then every 24 h. Waits for the next
/// tick when no IBKR_TWS session is live.
pub async fn run(pool: PgPool) {
    let Some(broker) = Broker::of("IBKR_TWS") else { return };
    let code = connection_code(&broker.environment());
    tokio::time::sleep(FIRST_RUN_DELAY).await;
    loop {
        match live_client(&code) {
            None => tracing::info!("futures sync skipped: no live IBKR_TWS session"),
            Some(client) => match sync_roots(&pool, client.as_ref()).await {
                Ok(r) => tracing::info!(roots = r.roots, contracts = r.contracts, skipped = r.skipped.len(), "futures sync done"),
                Err(e) => tracing::error!(error = %e, "futures sync failed"),
            },
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

#[utoipa::path(
    post, path = "/admin/futures/sync", tag = "admin",
    params(EnvParam),
    responses(
        (status = 200, description = "Contracts synced from IBKR", body = SyncReport),
        (status = 502, description = "IBKR or the database failed"),
        (status = 503, description = "IBKR gateway is not connected"),
    ),
    security(("bearer_token" = []))
)]
pub async fn sync_now(State(state): State<AppState>, Query(p): Query<EnvParam>) -> Result<Json<SyncReport>, AdminError> {
    let client = live(p.env.as_deref())?;
    sync_roots(state.pool(), client.as_ref())
        .await
        .map(Json)
        .map_err(|e| AdminError::new(StatusCode::BAD_GATEWAY, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};

    /// Answers `contract_details` from a list, filtered like IBKR does.
    struct Listed(Vec<TwsContract>);

    #[async_trait::async_trait]
    impl TwsClient for Listed {
        async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError> {
            Ok(self.0.iter().filter(|c| q.symbol.as_ref().is_none_or(|s| *s == c.symbol)).cloned().collect())
        }
        async fn next_order_id(&self) -> Result<i32, TwsError> { unimplemented!() }
        async fn place(&self, _: &gateway::connectors::ibkr_tws::client::TwsOrder) -> Result<(), TwsError> { unimplemented!() }
        async fn cancel(&self, _: i32) -> Result<(), TwsError> { unimplemented!() }
        async fn open_orders(&self) -> Result<Vec<(i32, String, gateway::connectors::ibkr_tws::client::TwsOrderState)>, TwsError> { unimplemented!() }
        async fn completed_orders(&self) -> Result<Vec<(i32, String, gateway::connectors::ibkr_tws::client::TwsOrderState)>, TwsError> { unimplemented!() }
        async fn executions(&self, _: &str, _: DateTime<Utc>) -> Result<Vec<gateway::connectors::ibkr_tws::client::TwsEvent>, TwsError> { unimplemented!() }
        async fn matching_symbols(&self, _: &str) -> Result<Vec<TwsContract>, TwsError> { unimplemented!() }
        async fn prev_close(&self, _: i64, _: SecType, _: &str) -> Result<Option<f64>, TwsError> { unimplemented!() }
        async fn managed_accounts(&self) -> Result<Vec<String>, TwsError> { unimplemented!() }
        fn events(&self) -> tokio::sync::broadcast::Receiver<gateway::connectors::ibkr_tws::client::TwsEvent> { unimplemented!() }
        fn connected(&self) -> bool { true }
        fn shutdown(&self) {}
    }

    fn fut(con_id: i64, root: &str, local: &str, y: i32, m: u32, d: u32) -> TwsContract {
        let day = NaiveDate::from_ymd_opt(y, m, d).unwrap();
        TwsContract {
            con_id,
            symbol: root.into(),
            exchange: "CME".into(),
            primary_exchange: String::new(),
            currency: "USD".into(),
            min_tick: 0.25,
            lot_size: None,
            name: root.into(),
            isin: None,
            sec_type: SecType::Fut,
            local_symbol: local.into(),
            multiplier: if root == "ES" { 50.0 } else { 5.0 },
            last_trade: Some(day),
            last_trade_at: Some(day.and_hms_opt(14, 30, 0).unwrap().and_utc()),
            strike: None,
            right: None,
        }
    }

    /// The tests save the same rows; concurrent upserts would deadlock.
    static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn isolated_pool() -> PgPool {
        let url = std::env::var("OMS_TEST_DATABASE_URL").expect("set OMS_TEST_DATABASE_URL to a disposable database");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect isolated test database")
    }

    async fn counts(pool: &PgPool) -> (i64, i64, i64) {
        let n = |sql: &'static str| async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.unwrap() };
        (
            n("SELECT count(*) FROM instrument WHERE instrument_class = 'FUTURE'").await,
            n("SELECT count(*) FROM broker_instrument WHERE broker_code = 'IBKR_TWS' AND native_id IN ('990005001','990005002','990005003')").await,
            n("SELECT count(*) FROM instrument_derivative").await,
        )
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn sync_upserts_every_listed_contract_of_each_root() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        let esz6 = fut(990_005_001, "ES", "ESZ6", 2036, 12, 18);
        let at = esz6.last_trade_at;
        let fake = Listed(vec![
            esz6,
            fut(990_005_002, "ES", "ESH7", 2037, 3, 19),
            fut(990_005_003, "MES", "MESZ6", 2036, 12, 18),
            // already expired: dropped, not saved
            fut(990_005_004, "ES", "ESH0", 2020, 3, 20),
        ]);
        let report = sync_roots(&pool, &fake).await.unwrap();
        assert_eq!((report.roots, report.contracts), (3, 3), "{report:?}");
        assert!(report.skipped.is_empty());

        let (class, size, under, expires): (String, Option<f64>, String, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT i.instrument_class, i.contract_size::float8, d.underlying_symbol, d.expires_at \
             FROM instrument i JOIN instrument_derivative d ON d.instrument_id = i.id \
             WHERE i.symbol = 'ESZ6' AND i.venue = 'XCME'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((class.as_str(), size, under.as_str(), expires), ("FUTURE", Some(50.0), "ES", at));
        let native: String = sqlx::query_scalar(
            "SELECT b.native_id FROM broker_instrument b JOIN instrument i ON i.id = b.instrument_id \
             WHERE b.broker_code = 'IBKR_TWS' AND i.symbol = 'ESZ6'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(native, "990005001");
        let expired: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument WHERE symbol = 'ESH0'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(expired, 0);
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn sync_is_idempotent() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        let fake = Listed(vec![
            fut(990_005_001, "ES", "ESZ6", 2036, 12, 18),
            fut(990_005_002, "ES", "ESH7", 2037, 3, 19),
            fut(990_005_003, "MES", "MESZ6", 2036, 12, 18),
        ]);
        sync_roots(&pool, &fake).await.unwrap();
        let before = counts(&pool).await;
        let again = sync_roots(&pool, &fake).await.unwrap();
        assert_eq!(again.contracts, 3);
        assert_eq!(counts(&pool).await, before);
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn an_unmappable_contract_is_skipped_and_the_rest_save() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        let mut bad = fut(990_005_002, "ES", "", 2037, 3, 19);
        bad.local_symbol.clear();
        let fake = Listed(vec![fut(990_005_001, "ES", "ESZ6", 2036, 12, 18), bad]);
        let report = sync_roots(&pool, &fake).await.unwrap();
        assert_eq!(report.contracts, 1);
        assert_eq!(report.skipped.len(), 1, "{report:?}");
    }
}
