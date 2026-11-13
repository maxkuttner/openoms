//! IBKR_TWS instrument import: admin endpoints that resolve a CSV or a search
//! against the live IB Gateway session and save chosen contracts by conId, and
//! the CLI client (`oms setup sync-broker --symbols file.csv`)
//! that drives them.
//!
//! Which gateway: `?env=` picks the connection environment (`PAPER` by
//! default). Saving always maps to broker `IBKR_TWS`, whatever the environment.

use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use gateway::adapter::{BrokerError, BrokerInstrument, InstrumentProvider};
use gateway::connectors::ibkr_tws::client::{ContractQuery, TwsClient, TwsContract, TwsError};
use gateway::connectors::ibkr_tws::exchanges::to_mic;
use gateway::connectors::ibkr_tws::resolve::{parse_csv, resolve, to_broker_instrument, Resolution};
use gateway::connectors::ibkr_tws::{connection_code, live_client};

use crate::admin::AdminError;
use crate::app_state::AppState;
use crate::setup::brokers::{load_universe, Broker, LoadOptions, LoadProgress, UniverseLoadReport};

const NOT_CONNECTED: &str = "IBKR gateway is not connected";
/// How many search hits are expanded into full contract details.
const SEARCH_LIMIT: usize = 10;
/// Most CSV rows or conIds one request may carry.
pub const MAX_ROWS: usize = 1000;
/// Largest request body the import routes accept.
const BODY_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EnvParam {
    /// Connection environment whose gateway to ask: `PAPER` (default) or `LIVE`.
    pub env: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchParams {
    /// Symbol or company-name fragment.
    pub q: String,
    /// Connection environment whose gateway to ask: `PAPER` (default) or `LIVE`.
    pub env: Option<String>,
}

/// A contract as IBKR describes it.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Contract {
    pub con_id: i64,
    pub symbol: String,
    /// IBKR primary exchange code, e.g. `IBIS`.
    pub primary_exchange: String,
    /// The OMS venue (ISO 10383 MIC) it maps to.
    pub mic: Option<String>,
    pub currency: String,
    pub min_tick: f64,
    pub lot_size: Option<f64>,
    pub name: String,
    pub isin: Option<String>,
}

impl From<&TwsContract> for Contract {
    fn from(c: &TwsContract) -> Self {
        Self {
            con_id: c.con_id,
            symbol: c.symbol.clone(),
            primary_exchange: c.primary_exchange.clone(),
            mic: to_mic(&c.primary_exchange).map(str::to_string),
            currency: c.currency.clone(),
            min_tick: c.min_tick,
            lot_size: c.lot_size,
            name: c.name.clone(),
            isin: c.isin.clone(),
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ImportResolveRequest {
    /// CSV text with a header: `symbol`, `exchange`, `currency`, `conid` (any order).
    pub csv: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResolvedRow {
    /// CSV line, 1-based including the header.
    pub line: usize,
    /// `found`, `ambiguous` or `not_found`.
    pub status: String,
    /// The one contract for `found`; every candidate for `ambiguous`; empty otherwise.
    pub candidates: Vec<Contract>,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SaveRequest {
    pub con_ids: Vec<i64>,
}

/// The admin routes; merged into the admin router, so they share its auth.
pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    with_body_limit(
        axum::Router::new()
            .route("/admin/brokers/IBKR_TWS/instruments", post(save))
            .route("/admin/brokers/IBKR_TWS/instruments/resolve", post(resolve_csv))
            .route("/admin/brokers/IBKR_TWS/instruments/search", get(search)),
    )
}

fn with_body_limit<S: Clone + Send + Sync + 'static>(r: axum::Router<S>) -> axum::Router<S> {
    r.layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
}

fn live(env: Option<&str>) -> Result<std::sync::Arc<dyn TwsClient>, AdminError> {
    let env = env.unwrap_or("PAPER").to_uppercase();
    live_client(&connection_code(&env)).ok_or_else(not_connected)
}

fn not_connected() -> AdminError {
    AdminError::new(StatusCode::SERVICE_UNAVAILABLE, NOT_CONNECTED)
}

/// A request that failed because the session dropped is "not connected", not a
/// per-row failure.
fn dropped(client: &dyn TwsClient) -> Result<(), AdminError> {
    if client.connected() {
        Ok(())
    } else {
        Err(not_connected())
    }
}

#[utoipa::path(
    post, path = "/admin/brokers/IBKR_TWS/instruments/resolve", tag = "admin",
    params(EnvParam),
    request_body = ImportResolveRequest,
    responses(
        (status = 200, description = "One entry per CSV row", body = [ResolvedRow]),
        (status = 422, description = "The CSV is malformed"),
        (status = 503, description = "IBKR gateway is not connected"),
    ),
    security(("bearer_token" = []))
)]
pub async fn resolve_csv(
    Query(p): Query<EnvParam>,
    Json(body): Json<ImportResolveRequest>,
) -> Result<Json<Vec<ResolvedRow>>, AdminError> {
    let client = live(p.env.as_deref())?;
    let rows = parse_csv(&body.csv).map_err(unprocessable)?;
    check_cap(rows.len(), "CSV rows")?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let (status, candidates, message) = match resolve(client.as_ref(), row).await {
            Resolution::Found(c) => ("found", vec![Contract::from(&c)], String::new()),
            Resolution::Ambiguous(cs) => (
                "ambiguous",
                cs.iter().map(Contract::from).collect(),
                format!("{} contracts match; add an exchange or currency, or a conid", cs.len()),
            ),
            Resolution::NotFound(m) => ("not_found", Vec::new(), m),
        };
        out.push(ResolvedRow { line: row.line, status: status.into(), candidates, message });
    }
    dropped(client.as_ref())?;
    Ok(Json(out))
}

#[utoipa::path(
    get, path = "/admin/brokers/IBKR_TWS/instruments/search", tag = "admin",
    params(SearchParams),
    responses(
        (status = 200, description = "Matching contracts on supported exchanges", body = [Contract]),
        (status = 503, description = "IBKR gateway is not connected"),
    ),
    security(("bearer_token" = []))
)]
pub async fn search(Query(p): Query<SearchParams>) -> Result<Json<Vec<Contract>>, AdminError> {
    let client = live(p.env.as_deref())?;
    let hits = client.matching_symbols(p.q.trim()).await.map_err(tws_error)?;
    let mut out = Vec::new();
    for hit in hits.iter().filter(|h| to_mic(&h.primary_exchange).is_some()).take(SEARCH_LIMIT) {
        // The symbol search carries no tick size; the details do.
        let q = ContractQuery { con_id: Some(hit.con_id), ..Default::default() };
        // A hit whose details fail is left out; the rest still answer.
        if let Ok(full) = client.contract_details(&q).await {
            if let Some(full) = full.first() {
                out.push(Contract::from(full));
            }
        }
    }
    dropped(client.as_ref())?;
    Ok(Json(out))
}

#[utoipa::path(
    post, path = "/admin/brokers/IBKR_TWS/instruments", tag = "admin",
    params(EnvParam),
    request_body = SaveRequest,
    responses(
        (status = 200, description = "What was saved", body = UniverseLoadReport),
        (status = 422, description = "A conId is unknown or unsupported; nothing was saved"),
        (status = 503, description = "IBKR gateway is not connected"),
    ),
    security(("bearer_token" = []))
)]
pub async fn save(
    State(state): State<AppState>,
    Query(p): Query<EnvParam>,
    Json(body): Json<SaveRequest>,
) -> Result<Json<UniverseLoadReport>, AdminError> {
    check_cap(body.con_ids.len(), "conIds")?;
    let client = live(p.env.as_deref())?;
    let mut instruments = Vec::new();
    for &con_id in &body.con_ids {
        let q = ContractQuery { con_id: Some(con_id), ..Default::default() };
        let found = client.contract_details(&q).await.map_err(tws_error)?;
        let contract = match found.as_slice() {
            [one] => one,
            _ => return Err(unprocessable(format!("conId {con_id}: IBKR has no single contract for it"))),
        };
        instruments.push(to_broker_instrument(contract).map_err(|e| unprocessable(format!("conId {con_id}: {e}")))?);
    }
    dropped(client.as_ref())?;
    let report = save_instruments(state.pool(), instruments, false)
        .await
        .map_err(|e| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

fn check_cap(n: usize, what: &str) -> Result<(), AdminError> {
    if n > MAX_ROWS {
        return Err(unprocessable(format!("{n} {what} is over the limit of {MAX_ROWS} per request")));
    }
    Ok(())
}

fn unprocessable(m: String) -> AdminError {
    AdminError::new(StatusCode::UNPROCESSABLE_ENTITY, m)
}

fn tws_error(e: TwsError) -> AdminError {
    match e {
        TwsError::NotConnected => not_connected(),
        e => AdminError::new(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

/// Hands a fixed list to `load_universe`, which upserts the master instruments
/// and maps them to `IBKR_TWS`. Saving the same contract again changes nothing.
struct Chosen(Mutex<Vec<BrokerInstrument>>);

#[async_trait::async_trait]
impl InstrumentProvider for Chosen {
    async fn list_instruments(&self, _: &[String]) -> Result<Vec<BrokerInstrument>, BrokerError> {
        Ok(std::mem::take(&mut *self.0.lock().unwrap()))
    }
}

struct Quiet;
impl LoadProgress for Quiet {
    fn progress(&self, _: i64, _: i64) {}
    fn log(&self, level: &str, message: &str) {
        if level == "warn" {
            tracing::warn!("{message}");
        } else {
            tracing::info!("{message}");
        }
    }
}

/// Saves resolved instruments for broker `IBKR_TWS`. Also the entry point
/// the nightly refresh uses.
pub async fn save_instruments(
    pool: &PgPool,
    instruments: Vec<BrokerInstrument>,
    no_enrich: bool,
) -> Result<UniverseLoadReport, Box<dyn std::error::Error>> {
    let broker = Broker::of("IBKR_TWS").ok_or("IBKR_TWS is not a universe broker")?;
    let opts = LoadOptions { no_enrich, allow_skips: true, underlyings: Vec::new() };
    load_universe(pool, broker, &Chosen(Mutex::new(instruments)), &opts, &Quiet).await
}

// ── Nightly refresh ──────────────────────────────────────────────────────────

/// IBKR answers a conId it no longer knows with error 200, "No security
/// definition has been found". That is the one error that means "vanished".
const NO_SECURITY_DEFINITION: i32 = 200;

/// What a refresh changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    /// Instruments re-saved from IBKR's current contract details.
    pub updated: usize,
    /// Routing rows newly marked `is_tradeable = false`.
    pub untradeable: usize,
}

/// Re-resolves every stored `IBKR_TWS` instrument by conId and re-saves what
/// IBKR still lists (new tick size, lot size, name). A conId IBKR no longer
/// has details for gets `broker_instrument.is_tradeable = false` (the flag the
/// order path reads), never a delete. Requests go one at a time. Any other
/// failure, such as the client being down, aborts the run before anything is
/// written: an outage is not a vanished contract.
pub async fn refresh_ibkr_instruments(pool: &PgPool, client: &dyn TwsClient) -> RefreshReport {
    match refresh(pool, client).await {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(error = %e, "IBKR instrument refresh skipped");
            RefreshReport::default()
        }
    }
}

async fn refresh(pool: &PgPool, client: &dyn TwsClient) -> Result<RefreshReport, Box<dyn std::error::Error>> {
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT native_id FROM broker_instrument WHERE broker_code = 'IBKR_TWS' AND native_id IS NOT NULL ORDER BY native_id",
    )
    .fetch_all(pool)
    .await?;

    let mut current = Vec::new();
    let mut vanished = Vec::new();
    for native_id in stored {
        let Ok(con_id) = native_id.parse::<i64>() else {
            tracing::warn!(native_id, "IBKR instrument refresh: native_id is not a conId; left alone");
            continue;
        };
        let q = ContractQuery { con_id: Some(con_id), ..Default::default() };
        match client.contract_details(&q).await {
            Ok(found) => match found.first() {
                Some(c) => match to_broker_instrument(c) {
                    Ok(bi) => current.push(bi),
                    Err(e) => tracing::warn!(con_id, error = %e, "IBKR instrument refresh: contract not savable; left alone"),
                },
                None => vanished.push(native_id),
            },
            Err(TwsError::Api { code: NO_SECURITY_DEFINITION, .. }) => vanished.push(native_id),
            // IBKR answered, just not for this conId: one bad contract must not
            // stall the refresh of every other one.
            Err(e @ TwsError::Api { .. }) => tracing::warn!(con_id, error = %e, "IBKR instrument refresh: lookup refused; left alone"),
            // Transport trouble: stop before writing anything.
            Err(e) => return Err(format!("conId {con_id}: {e}").into()),
        }
    }

    let updated = if current.is_empty() { 0 } else { save_instruments(pool, current, true).await?.mapped as usize };
    let untradeable = if vanished.is_empty() {
        0
    } else {
        sqlx::query(
            "UPDATE broker_instrument SET is_tradeable = false, updated_at = now() \
             WHERE broker_code = 'IBKR_TWS' AND native_id = ANY($1) AND is_tradeable",
        )
        .bind(&vanished)
        .execute(pool)
        .await?
        .rows_affected() as usize
    };
    Ok(RefreshReport { updated, untradeable })
}

/// Hour (UTC) the refresh runs each day.
const REFRESH_HOUR_UTC: u32 = 6;

/// The next 06:00 UTC strictly after `now`.
fn next_refresh_after(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    let today = now.date_naive().and_hms_opt(REFRESH_HOUR_UTC, 0, 0).expect("valid time").and_utc();
    if today > now {
        today
    } else {
        today + chrono::Duration::days(1)
    }
}

/// Daily 06:00 UTC refresh. Silent when no IBKR_TWS session is live at the time.
pub async fn run_nightly(pool: PgPool) {
    let Some(broker) = Broker::of("IBKR_TWS") else { return };
    let code = connection_code(&broker.environment());
    loop {
        let now = chrono::Utc::now();
        let wait = (next_refresh_after(now) - now).to_std().unwrap_or_default();
        tokio::time::sleep(wait).await;
        let Some(client) = live_client(&code) else { continue };
        let report = refresh_ibkr_instruments(&pool, client.as_ref()).await;
        tracing::info!(updated = report.updated, untradeable = report.untradeable, "IBKR instrument refresh done");
    }
}

// ── CLI ──────────────────────────────────────────────────────────────────────

/// `sync-broker --symbols file.csv`: resolve through the running OMS, save the
/// unambiguous rows, list the rest by line. Errors (exit 1) when any row needs
/// attention. Talks to the OMS at the configured bind address with the admin
/// password resolved as the server resolves it; never starts Docker.
pub async fn run_cli(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let env = Broker::of("IBKR_TWS").ok_or("IBKR_TWS connector missing")?.environment();
    let env = env.as_str();
    let csv = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file = crate::config::load();
    let bind = crate::resolve_bind_addr(std::env::var("OMS_BIND_ADDR").ok(), file);
    let base = format!("http://{}", crate::setup::status::reachable_addr(&bind));
    let password = crate::admin_bootstrap::read_host_token()?;
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300)).build()?;
    let root = format!("{base}/admin/brokers/IBKR_TWS/instruments");

    let rows: Vec<ResolvedRow> = admin_call(
        http.post(format!("{root}/resolve?env={env}")).bearer_auth(&password).json(&serde_json::json!({ "csv": csv })),
    )
    .await?;

    let con_ids = savable_con_ids(&rows);
    let mut problems = 0usize;
    for row in &rows {
        match (row.status.as_str(), row.candidates.as_slice()) {
            ("found", _) => {}
            ("ambiguous", cands) => {
                problems += 1;
                println!("line {}: ambiguous, {}", row.line, row.message);
                for c in cands {
                    println!(
                        "    conid {}  {} on {} ({})  {}",
                        c.con_id, c.symbol, c.primary_exchange, c.currency, c.name
                    );
                }
            }
            _ => {
                problems += 1;
                println!("line {}: not found, {}", row.line, row.message);
            }
        }
    }

    if !con_ids.is_empty() {
        let report: serde_json::Value = admin_call(
            http.post(format!("{root}?env={env}")).bearer_auth(&password).json(&SaveRequest { con_ids: con_ids.clone() }),
        )
        .await?;
        println!("saved {} instrument(s): {report}", con_ids.len());
    }
    if problems > 0 {
        return Err(format!("{problems} row(s) need attention; nothing for them was saved").into());
    }
    Ok(())
}

/// The conIds safe to save: unique `found` rows only. Ambiguous and not-found
/// rows contribute nothing.
fn savable_con_ids(rows: &[ResolvedRow]) -> Vec<i64> {
    let mut ids = Vec::new();
    for row in rows {
        if let ("found", [c]) = (row.status.as_str(), row.candidates.as_slice()) {
            if !ids.contains(&c.con_id) {
                ids.push(c.con_id);
            }
        }
    }
    ids
}

async fn admin_call<T: serde::de::DeserializeOwned>(req: reqwest::RequestBuilder) -> Result<T, Box<dyn std::error::Error>> {
    let resp = req.send().await.map_err(|e| format!("could not reach the OMS (is it running?): {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("the OMS answered {status}: {body}").into());
    }
    Ok(resp.json().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(con_id: i64, primary: &str) -> TwsContract {
        TwsContract {
            con_id,
            symbol: "SAP".into(),
            exchange: "SMART".into(),
            primary_exchange: primary.into(),
            currency: "EUR".into(),
            min_tick: 0.01,
            lot_size: None,
            name: "SAP SE".into(),
            isin: None,
        }
    }

    fn rrow(line: usize, status: &str, ids: &[i64]) -> ResolvedRow {
        ResolvedRow {
            line,
            status: status.into(),
            candidates: ids.iter().map(|i| Contract::from(&contract(*i, "IBIS"))).collect(),
            message: String::new(),
        }
    }

    #[test]
    fn only_unique_found_rows_are_saved() {
        let rows = [
            rrow(2, "found", &[1]),
            rrow(3, "ambiguous", &[2, 3]),
            rrow(4, "not_found", &[]),
            rrow(5, "found", &[1]),
            rrow(6, "found", &[4]),
        ];
        assert_eq!(savable_con_ids(&rows), vec![1, 4]);
    }

    #[test]
    fn requests_over_the_cap_are_refused_naming_the_limit() {
        assert!(check_cap(MAX_ROWS, "CSV rows").is_ok());
        let e = check_cap(MAX_ROWS + 1, "conIds").unwrap_err();
        assert!(format!("{e:?}").contains("1000"));
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        use axum::body::Body;
        use tower::Service;
        let mut app = with_body_limit::<()>(axum::Router::new().route(
            "/",
            axum::routing::post(|Json(_): Json<ImportResolveRequest>| async {}),
        ));
        let post = |csv: String| {
            axum::http::Request::post("/")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "csv": csv }).to_string()))
                .unwrap()
        };
        let ok = app.call(post("symbol\nAAPL\n".into())).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let big = app.call(post("x".repeat(BODY_LIMIT + 1))).await.unwrap();
        assert_eq!(big.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn contract_view_carries_the_mic() {
        assert_eq!(Contract::from(&contract(2, "IBIS")).mic.as_deref(), Some("XETR"));
        assert_eq!(Contract::from(&contract(3, "MOON")).mic, None);
    }

    #[test]
    fn refresh_runs_at_0600_utc() {
        use chrono::TimeZone;
        let at = |d, h, m| chrono::Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        assert_eq!(next_refresh_after(at(10, 5, 59)), at(10, 6, 0));
        assert_eq!(next_refresh_after(at(10, 6, 0)), at(11, 6, 0));
        assert_eq!(next_refresh_after(at(10, 23, 0)), at(11, 6, 0));
    }

    /// Answers `contract_details` from a map; a missing conId is "no result",
    /// a conId in `refused` an IB API error, and `down` makes every call a
    /// transport error.
    #[derive(Default)]
    struct Scripted {
        details: std::collections::HashMap<i64, TwsContract>,
        refused: std::collections::HashSet<i64>,
        down: bool,
    }

    #[async_trait::async_trait]
    impl TwsClient for Scripted {
        async fn contract_details(&self, q: &ContractQuery) -> Result<Vec<TwsContract>, TwsError> {
            if self.down {
                return Err(TwsError::NotConnected);
            }
            if self.refused.contains(&q.con_id.unwrap()) {
                return Err(TwsError::Api { code: 321, message: "Error validating request".into() });
            }
            Ok(self.details.get(&q.con_id.unwrap()).cloned().into_iter().collect())
        }
        async fn next_order_id(&self) -> Result<i32, TwsError> { unimplemented!() }
        async fn place(&self, _: &gateway::connectors::ibkr_tws::client::TwsOrder) -> Result<(), TwsError> { unimplemented!() }
        async fn cancel(&self, _: i32) -> Result<(), TwsError> { unimplemented!() }
        async fn open_orders(&self) -> Result<Vec<(i32, String, gateway::connectors::ibkr_tws::client::TwsOrderState)>, TwsError> { unimplemented!() }
        async fn completed_orders(&self) -> Result<Vec<(i32, String, gateway::connectors::ibkr_tws::client::TwsOrderState)>, TwsError> { unimplemented!() }
        async fn executions_today(&self) -> Result<Vec<gateway::connectors::ibkr_tws::client::TwsEvent>, TwsError> { unimplemented!() }
        async fn matching_symbols(&self, _: &str) -> Result<Vec<TwsContract>, TwsError> { unimplemented!() }
        async fn prev_close(&self, _: i64) -> Result<Option<f64>, TwsError> { unimplemented!() }
        async fn managed_accounts(&self) -> Result<Vec<String>, TwsError> { unimplemented!() }
        fn events(&self) -> tokio::sync::broadcast::Receiver<gateway::connectors::ibkr_tws::client::TwsEvent> { unimplemented!() }
        fn connected(&self) -> bool { !self.down }
        fn shutdown(&self) {}
    }

    /// A refresh covers every stored IBKR instrument, so the tests that run one
    /// must not overlap.
    static REFRESH_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn tradeable(pool: &PgPool, con_id: i64) -> bool {
        sqlx::query_scalar("SELECT is_tradeable FROM broker_instrument WHERE broker_code = 'IBKR_TWS' AND native_id = $1")
            .bind(con_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_vanished_con_id_becomes_untradeable_not_deleted() {
        let _serial = REFRESH_TESTS.lock().await;
        let pool = isolated_pool().await;
        let (kept, gone) = (990_000_011_i64, 990_000_012_i64);
        let (mut a, mut b) = (contract(kept, "IBIS"), contract(gone, "IBIS"));
        a.symbol = "ZKEEP".into();
        b.symbol = "ZGONE".into();
        save_instruments(&pool, vec![to_broker_instrument(&a).unwrap(), to_broker_instrument(&b).unwrap()], true)
            .await
            .unwrap();

        // An outage marks nothing, even though no conId answers.
        let down = Scripted { down: true, ..Default::default() };
        assert_eq!(refresh_ibkr_instruments(&pool, &down).await, RefreshReport::default());
        assert!(tradeable(&pool, gone).await);

        let client = Scripted { details: [(kept, a)].into(), ..Default::default() };
        let report = refresh_ibkr_instruments(&pool, &client).await;
        assert!(report.untradeable >= 1, "{report:?}");
        assert!(tradeable(&pool, kept).await);
        assert!(!tradeable(&pool, gone).await, "the vanished conId stays, untradeable");
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_con_id_ibkr_refuses_is_skipped_and_the_rest_refreshed() {
        let _serial = REFRESH_TESTS.lock().await;
        let pool = isolated_pool().await;
        let (refused, ok) = (990_000_031_i64, 990_000_032_i64);
        let (mut a, mut b) = (contract(refused, "IBIS"), contract(ok, "IBIS"));
        a.symbol = "ZREFUSED".into();
        b.symbol = "ZOKTICK".into();
        save_instruments(&pool, vec![to_broker_instrument(&a).unwrap(), to_broker_instrument(&b).unwrap()], true)
            .await
            .unwrap();

        b.min_tick = 0.05;
        let client = Scripted { details: [(ok, b)].into(), refused: [refused].into(), ..Default::default() };
        let report = refresh_ibkr_instruments(&pool, &client).await;
        assert!(report.updated >= 1, "{report:?}");
        assert!(tradeable(&pool, refused).await, "a refused lookup is not a vanished contract");
        let tick: f64 = sqlx::query_scalar("SELECT price_increment::float8 FROM instrument WHERE symbol = 'ZOKTICK' AND venue = 'XETR'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tick, 0.05);
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn a_changed_min_tick_is_updated() {
        let _serial = REFRESH_TESTS.lock().await;
        let pool = isolated_pool().await;
        let con_id = 990_000_021_i64;
        let mut c = contract(con_id, "IBIS");
        c.symbol = "ZTICK".into();
        save_instruments(&pool, vec![to_broker_instrument(&c).unwrap()], true).await.unwrap();

        c.min_tick = 0.05;
        let client = Scripted { details: [(con_id, c)].into(), ..Default::default() };
        let report = refresh_ibkr_instruments(&pool, &client).await;
        assert!(report.updated >= 1, "{report:?}");
        let tick: f64 = sqlx::query_scalar("SELECT price_increment::float8 FROM instrument WHERE symbol = 'ZTICK' AND venue = 'XETR'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tick, 0.05);
    }

    /// A disposable, already-migrated database (never the dev one).
    async fn isolated_pool() -> PgPool {
        let url = std::env::var("COLO_TEST_DATABASE_URL").expect("set COLO_TEST_DATABASE_URL to a disposable database");
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

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn saving_twice_is_idempotent() {
        let pool = isolated_pool().await;
        // A conId no real run would use, so reruns and the shared CI database agree.
        let con_id = 990_000_001_i64;
        let mut c = contract(con_id, "IBIS");
        c.symbol = "ZTEST".into();
        let count = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM broker_instrument WHERE broker_code = 'IBKR_TWS' AND native_id = $1",
            )
            .bind(con_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let master = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM instrument WHERE symbol = 'ZTEST' AND venue = 'XETR'")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        let first = save_instruments(&pool, vec![to_broker_instrument(&c).unwrap()], true).await.unwrap();
        assert_eq!((first.mapped, first.skipped), (1, 0), "{first:?}");
        let second = save_instruments(&pool, vec![to_broker_instrument(&c).unwrap()], true).await.unwrap();
        assert_eq!((second.mapped, second.skipped), (1, 0));
        assert_eq!(count(pool.clone()).await, 1);
        assert_eq!(master(pool.clone()).await, 1);
    }
}
