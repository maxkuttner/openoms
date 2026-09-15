use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::engine::{general_purpose, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::info;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::credentials::{BrokerCredentials, Connection, CredentialState, FeedCredentials};
use crate::credentials_api::{self, CredentialSubmission, TestOutcome};
use crate::reload;
use crate::stream_health::StreamHealth;
use crate::domain::identity::{Account, BrokerConnection, Portfolio, Grant, Principal};
use crate::symbology_resolver::{self, ResolveError, ResolveOutcome};
use symbology::InstrumentQuery;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreatePrincipal {
    pub code: String,
    pub principal_type: String,
    pub external_subject: Option<String>,
    pub display_name: Option<String>,
    pub status: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdatePrincipal {
    pub code: Option<String>,
    pub principal_type: Option<String>,
    pub external_subject: Option<String>,
    pub display_name: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreatePortfolio {
    pub code: String,
    pub name: String,
    pub status: String,
    pub base_currency: Option<String>,
    pub default_account_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdatePortfolio {
    pub code: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub base_currency: Option<String>,
    pub default_account_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateAccount {
    pub code: String,
    pub broker_connection_code: String,
    pub external_account_ref: String,
    pub status: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateAccount {
    pub code: Option<String>,
    pub broker_connection_code: Option<String>,
    pub external_account_ref: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateBrokerConnection {
    pub code: String,
    pub broker_code: String,
    pub environment: String,
    pub status: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateBrokerConnection {
    pub broker_code: Option<String>,
    pub environment: Option<String>,
    pub status: Option<String>,
}

/// The credential view safe to put on the wire. `state` mirrors
/// `CredentialState` so the cockpit can tell "needs setup" (`unconfigured`)
/// apart from "stored but the master key does not open it" (`error`) — the
/// distinction the whole credential store was built to preserve; collapsing
/// them would invite an operator to re-enter a credential that is already
/// there instead of fixing the key. `fields` is populated only when `state`
/// is `"configured"`; `message` only when `state` is `"error"`. Never carries
/// a decrypted secret, in either state.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RedactedCredentials {
    pub code: String,
    /// "configured" | "unconfigured" | "error"
    pub state: String,
    pub fields: Vec<crate::credentials::RedactedField>,
    /// The reason a stored blob could not be used — set only when
    /// `state == "error"`, and always a description, never the payload.
    pub message: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[utoipa::path(
    post, path = "/admin/principals", tag = "admin",
    request_body = CreatePrincipal,
    responses(
        (status = 200, description = "Created", body = Principal),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_principal(
    State(state): State<AppState>,
    Json(payload): Json<CreatePrincipal>,
) -> Result<Json<Principal>, AdminError> {
    info!(code = %payload.code, principal_type = %payload.principal_type, "admin create principal");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Principal>(
        r#"
        INSERT INTO principal (
            id,
            code,
            principal_type,
            external_subject,
            display_name,
            status
        ) VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, code, principal_type, external_subject, display_name, status, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.principal_type)
    .bind(payload.external_subject)
    .bind(payload.display_name)
    .bind(payload.status)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct PrincipalFilter {
    pub external_subject: Option<String>,
}

#[utoipa::path(
    get, path = "/admin/principals", tag = "admin",
    params(PrincipalFilter),
    responses(
        (status = 200, description = "OK", body = [Principal]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_principals(
    State(state): State<AppState>,
    Query(filter): Query<PrincipalFilter>,
) -> Result<Json<Vec<Principal>>, AdminError> {
    info!("admin list principals");
    let records = if let Some(sub) = filter.external_subject {
        sqlx::query_as::<_, Principal>(
            r#"
            SELECT id, code, principal_type, external_subject, display_name, status, created_at, updated_at
            FROM principal
            WHERE external_subject = $1
            "#,
        )
        .bind(sub)
        .fetch_all(state.pool())
        .await
        .map_err(map_db_error)?
    } else {
        sqlx::query_as::<_, Principal>(
            r#"
            SELECT id, code, principal_type, external_subject, display_name, status, created_at, updated_at
            FROM principal
            ORDER BY created_at DESC
            "#,
        )
        .fetch_all(state.pool())
        .await
        .map_err(map_db_error)?
    };

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/principals/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    responses(
        (status = 200, description = "OK", body = Principal),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_principal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Principal>, AdminError> {
    info!(principal_id = %id, "admin get principal");
    let record = sqlx::query_as::<_, Principal>(
        r#"
        SELECT id, code, principal_type, external_subject, display_name, status, created_at, updated_at
        FROM principal
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("principal"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/principals/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    request_body = UpdatePrincipal,
    responses(
        (status = 200, description = "Updated", body = Principal),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_principal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdatePrincipal>,
) -> Result<Json<Principal>, AdminError> {
    info!(principal_id = %id, "admin update principal");
    let record = sqlx::query_as::<_, Principal>(
        r#"
        UPDATE principal
        SET
            code = COALESCE($1, code),
            principal_type = COALESCE($2, principal_type),
            external_subject = COALESCE($3, external_subject),
            display_name = COALESCE($4, display_name),
            status = COALESCE($5, status),
            updated_at = now()
        WHERE id = $6
        RETURNING id, code, principal_type, external_subject, display_name, status, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.principal_type)
    .bind(payload.external_subject)
    .bind(payload.display_name)
    .bind(payload.status)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("principal"))?;

    Ok(Json(record))
}

#[utoipa::path(
    post, path = "/admin/portfolios", tag = "admin",
    request_body = CreatePortfolio,
    responses(
        (status = 200, description = "Created", body = Portfolio),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_portfolio(
    State(state): State<AppState>,
    Json(payload): Json<CreatePortfolio>,
) -> Result<Json<Portfolio>, AdminError> {
    info!(code = %payload.code, name = %payload.name, "admin create portfolio");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Portfolio>(
        r#"
        INSERT INTO portfolio (
            id,
            code,
            name,
            status,
            base_currency,
            default_account_id
        ) VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, code, name, status, base_currency, default_account_id, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.name)
    .bind(payload.status)
    .bind(payload.base_currency)
    .bind(payload.default_account_id)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/portfolios", tag = "admin",
    responses(
        (status = 200, description = "OK", body = [Portfolio]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_portfolios(
    State(state): State<AppState>,
) -> Result<Json<Vec<Portfolio>>, AdminError> {
    info!("admin list portfolios");
    let records = sqlx::query_as::<_, Portfolio>(
        r#"
        SELECT id, code, name, status, base_currency, default_account_id, created_at, updated_at
        FROM portfolio
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/portfolios/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Portfolio ID")),
    responses(
        (status = 200, description = "OK", body = Portfolio),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_portfolio(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Portfolio>, AdminError> {
    info!(portfolio_id = %id, "admin get portfolio");
    let record = sqlx::query_as::<_, Portfolio>(
        r#"
        SELECT id, code, name, status, base_currency, default_account_id, created_at, updated_at
        FROM portfolio
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("portfolio"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/portfolios/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Portfolio ID")),
    request_body = UpdatePortfolio,
    responses(
        (status = 200, description = "Updated", body = Portfolio),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_portfolio(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdatePortfolio>,
) -> Result<Json<Portfolio>, AdminError> {
    info!(portfolio_id = %id, "admin update portfolio");
    let record = sqlx::query_as::<_, Portfolio>(
        r#"
        UPDATE portfolio
        SET
            code = COALESCE($1, code),
            name = COALESCE($2, name),
            status = COALESCE($3, status),
            base_currency = COALESCE($4, base_currency),
            default_account_id = COALESCE($5, default_account_id),
            updated_at = now()
        WHERE id = $6
        RETURNING id, code, name, status, base_currency, default_account_id, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.name)
    .bind(payload.status)
    .bind(payload.base_currency)
    .bind(payload.default_account_id)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("portfolio"))?;

    Ok(Json(record))
}

#[utoipa::path(
    post, path = "/admin/accounts", tag = "admin",
    request_body = CreateAccount,
    responses(
        (status = 200, description = "Created", body = Account),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(code = %payload.code, broker_connection_code = %payload.broker_connection_code, "admin create account");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO account (
            id,
            code,
            broker_connection_code,
            external_account_ref,
            status
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING id, code, broker_connection_code, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.broker_connection_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/accounts", tag = "admin",
    responses(
        (status = 200, description = "OK", body = [Account]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<Vec<Account>>, AdminError> {
    info!("admin list accounts");
    let records = sqlx::query_as::<_, Account>(
        r#"
        SELECT id, code, broker_connection_code, external_account_ref, status, created_at, updated_at
        FROM account
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/accounts/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Account ID")),
    responses(
        (status = 200, description = "OK", body = Account),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Account>, AdminError> {
    info!(account_id = %id, "admin get account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        SELECT id, code, broker_connection_code, external_account_ref, status, created_at, updated_at
        FROM account
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("account"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/accounts/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Account ID")),
    request_body = UpdateAccount,
    responses(
        (status = 200, description = "Updated", body = Account),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(account_id = %id, "admin update account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        UPDATE account
        SET
            code = COALESCE($1, code),
            broker_connection_code = COALESCE($2, broker_connection_code),
            external_account_ref = COALESCE($3, external_account_ref),
            status = COALESCE($4, status),
            updated_at = now()
        WHERE id = $5
        RETURNING id, code, broker_connection_code, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.broker_connection_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("account"))?;

    Ok(Json(record))
}

// ── Broker connections ────────────────────────────────────────────────────────

/// Rejects a `broker_code` the adapter registry could never be reached
/// under. `broker_code` is unconstrained `TEXT` — no CHECK, no FK — but
/// `reload.rs` registers adapters under hardcoded literals (`"IBKR"`,
/// `"BINANCE"`, …), keyed by this same column. A code that saves clean here
/// but doesn't match one of those literals passes reload silently and then
/// 503s every order routed through it — see `setup::brokers::known_broker_codes`
/// for why the accepted set is sourced from there rather than repeated here.
fn validate_broker_code(code: &str) -> Result<(), AdminError> {
    let known = crate::setup::brokers::known_broker_codes();
    if known.contains(&code) {
        Ok(())
    } else {
        Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: format!("broker_code must be one of: {} (got {code:?})", known.join(", ")),
        })
    }
}

#[utoipa::path(
    post, path = "/admin/broker-connections", tag = "admin",
    request_body = CreateBrokerConnection,
    responses(
        (status = 200, description = "Created", body = BrokerConnection),
        (status = 400, description = "Invalid environment (must be PAPER or LIVE)"),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_broker_connection(
    State(state): State<AppState>,
    Json(payload): Json<CreateBrokerConnection>,
) -> Result<Json<BrokerConnection>, AdminError> {
    if payload.environment != "PAPER" && payload.environment != "LIVE" {
        return Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: "environment must be PAPER or LIVE".to_string(),
        });
    }
    validate_broker_code(&payload.broker_code)?;
    info!(code = %payload.code, broker_code = %payload.broker_code, environment = %payload.environment, "admin create broker connection");
    let record = sqlx::query_as::<_, BrokerConnection>(
        r#"
        INSERT INTO broker_connection (code, broker_code, environment, status)
        VALUES ($1, $2, $3, $4)
        RETURNING code, broker_code, environment, status, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.broker_code)
    .bind(payload.environment)
    .bind(payload.status)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

/// Live health of the broker/exchange WebSocket streams (in-memory, ephemeral).
pub async fn list_stream_health(State(state): State<AppState>) -> Json<Vec<StreamHealth>> {
    Json(state.stream_health().snapshot())
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct FeedSummary {
    pub feed_code: String,
    pub instrument_class: String,
    /// Ranked failover preference within the class; lower wins.
    pub rank: i32,
    pub enabled: bool,
    /// How many active instruments this feed's symbology covers, for the class.
    /// Derived from the feed's own `candidates()` filter at request time.
    pub mapped_instruments: i64,
}

/// The configured data feeds: the ranked market-data source policy plus how many
/// instruments each currently prices. Distinct from broker connections (execution).
#[utoipa::path(
    get, path = "/admin/feeds", tag = "admin",
    responses((status = 200, description = "OK", body = [FeedSummary])),
    security(("bearer_token" = []))
)]
pub async fn list_feeds(
    State(state): State<AppState>,
) -> Result<Json<Vec<FeedSummary>>, AdminError> {
    let policies = sqlx::query_as::<_, (String, String, i32, bool)>(
        "SELECT source_code, instrument_class, rank, enabled \
         FROM oms.provider_feed_policy \
         ORDER BY source_code, rank",
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    // `mapped_instruments` is derived, not stored: count the catalog slice this
    // feed's symbology covers, narrowed to the policy row's instrument_class. A
    // policy naming a feed this build doesn't ship counts zero rather than 404ing —
    // the row is still real and the operator should see it.
    let mut rows = Vec::with_capacity(policies.len());
    for (feed_code, instrument_class, rank, enabled) in policies {
        let mapped_instruments = match crate::feeds::by_code(&feed_code) {
            Some(feed) => count_priceable(state.pool(), feed, &instrument_class)
                .await
                .map_err(map_db_error)?,
            None => 0,
        };
        rows.push(FeedSummary { feed_code, instrument_class, rank, enabled, mapped_instruments });
    }
    Ok(Json(rows))
}

/// How many active instruments of `instrument_class` this feed's symbology covers.
///
/// Counts what `candidates()` selects; it does not run `to_feed_symbol` over the
/// catalog, so an instrument the feed would decline is still counted. That keeps
/// this a single `COUNT` rather than a full scan into Rust, and the two only differ
/// for malformed symbols — which `preflight` reports separately.
async fn count_priceable(
    pool: &sqlx::PgPool,
    feed: &dyn dataprovider::FeedSymbology,
    instrument_class: &str,
) -> Result<i64, sqlx::Error> {
    let mut sql = String::from("SELECT count(*) FROM instrument WHERE status = 'ACTIVE' AND instrument_class = $1");
    let binds = feed.candidates().push_conditions(&mut sql, 2);

    let mut query = sqlx::query_scalar::<_, i64>(&sql).bind(instrument_class);
    for b in &binds {
        query = query.bind(*b);
    }
    query.fetch_one(pool).await
}

#[utoipa::path(
    get, path = "/admin/broker-connections", tag = "admin",
    responses(
        (status = 200, description = "OK", body = [BrokerConnection]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_broker_connections(
    State(state): State<AppState>,
) -> Result<Json<Vec<BrokerConnection>>, AdminError> {
    info!("admin list broker connections");
    let records = sqlx::query_as::<_, BrokerConnection>(
        r#"
        SELECT code, broker_code, environment, status, created_at, updated_at
        FROM broker_connection
        ORDER BY code
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/broker-connections/{code}", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 200, description = "OK", body = BrokerConnection),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_broker_connection(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<BrokerConnection>, AdminError> {
    info!(broker_connection_code = %code, "admin get broker connection");
    let record = sqlx::query_as::<_, BrokerConnection>(
        r#"
        SELECT code, broker_code, environment, status, created_at, updated_at
        FROM broker_connection
        WHERE code = $1
        "#,
    )
    .bind(code)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("broker_connection"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/broker-connections/{code}", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    request_body = UpdateBrokerConnection,
    responses(
        (status = 200, description = "Updated", body = BrokerConnection),
        (status = 400, description = "Invalid environment"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_broker_connection(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(payload): Json<UpdateBrokerConnection>,
) -> Result<Json<BrokerConnection>, AdminError> {
    if let Some(ref env) = payload.environment {
        if env != "PAPER" && env != "LIVE" {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: "environment must be PAPER or LIVE".to_string(),
            });
        }
    }
    if let Some(ref broker_code) = payload.broker_code {
        validate_broker_code(broker_code)?;
    }
    info!(broker_connection_code = %code, "admin update broker connection");
    let record = sqlx::query_as::<_, BrokerConnection>(
        r#"
        UPDATE broker_connection
        SET
            broker_code = COALESCE($1, broker_code),
            environment = COALESCE($2, environment),
            status = COALESCE($3, status),
            updated_at = now()
        WHERE code = $4
        RETURNING code, broker_code, environment, status, created_at, updated_at
        "#,
    )
    .bind(payload.broker_code)
    .bind(payload.environment)
    .bind(payload.status)
    .bind(code)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("broker_connection"))?;

    Ok(Json(record))
}

/// Resolves the configured master key the same way every credential endpoint
/// must: absent is fine — every row still reads back correctly as
/// Unconfigured-if-null / Error-if-not — but a *present and invalid* key is
/// reported rather than silently folded into "no key configured"; a wrong
/// key must not be hidden behind that story. Shared so `GET`, `PUT`,
/// `DELETE`, `test`, and `reload_connections` cannot each resolve this a
/// different way.
fn resolve_master_key() -> Result<Option<crate::secrets::MasterKey>, AdminError> {
    match crate::config::master_key(crate::config::load()) {
        Some(Ok(k)) => Ok(Some(k)),
        Some(Err(e)) => Err(AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("master key is invalid: {e}"),
        }),
        None => Ok(None),
    }
}

/// Maps one loaded connection's credential state onto the wire shape. Split
/// out from the handler so the mapping — the part that must never let a
/// secret through — is exercised directly in tests, with no database.
///
/// Generic over `T: ToRedactedFields` rather than pinned to
/// `BrokerCredentials`: brokers and feeds share this exact mapping (only
/// which fields are secret differs, and that lives in each type's own
/// `Redact` impl), so a feed connection reuses this function instead of
/// admin.rs growing a second copy of it.
fn redact_connection<T: crate::credentials::ToRedactedFields>(conn: Connection<T>) -> RedactedCredentials {
    let (state, fields, message) = match conn.credentials {
        CredentialState::Configured(c) => ("configured", c.to_redacted_fields(), None),
        CredentialState::Unconfigured => ("unconfigured", Vec::new(), None),
        CredentialState::Error(e) => ("error", Vec::new(), Some(e)),
    };
    RedactedCredentials {
        code: conn.code,
        state: state.to_string(),
        fields,
        message,
        updated_at: conn.credentials_updated_at,
    }
}

/// What is configured for one broker connection, with every secret withheld.
///
/// 404 only when the connection row itself does not exist. A connection with
/// no credentials stored is still 200, `state: "unconfigured"` — it exists,
/// it just needs setup, which is a different operator situation from 404
/// ("no such connection") and from `state: "error"` ("stored, but the master
/// key does not open it"). See `RedactedCredentials`.
#[utoipa::path(
    get, path = "/admin/broker-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 200, description = "OK — configured, unconfigured, or error; never a decrypted secret", body = RedactedCredentials),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, or the configured master key is invalid"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_broker_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<RedactedCredentials>, AdminError> {
    info!(broker_connection_code = %code, "admin get broker connection credentials");

    let master = resolve_master_key()?;

    let connections = crate::credentials::load_brokers(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;

    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("broker_connection"))?;

    Ok(Json(redact_connection(conn)))
}

/// Response body for a successful credential save.
///
/// `reload` is this connection's own entry out of the reload report the save
/// already triggered — not a second call the cockpit has to make — so the UI
/// can say "applied" (a REST adapter, swapped live) or "restart required" (a
/// FIX session) in the same response that confirms the write.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SaveResponse {
    pub redacted: RedactedCredentials,
    /// Whether the credential was actually checked against the broker before
    /// being written — `false` for FIX (see `credentials_api::test_broker`),
    /// so the UI reports "not testable" rather than implying a pass it never
    /// earned.
    pub tested: bool,
    /// Present exactly when `tested` is `false`: `TestOutcome::NotTestable`'s
    /// own explanation (e.g. "FIX credentials are validated at session
    /// logon, not before save"), passed through so the cockpit renders the
    /// server's reason instead of hardcoding its own guess at the wording.
    /// Always `None` when `tested` is `true` — a `Failed` test never reaches
    /// this response at all (422, before anything is written).
    pub message: Option<String>,
    /// `None` only if the reload report (queried by this same connection's
    /// code, right after the save that triggered it) somehow lacks an entry
    /// for it — never expected in practice, but a missing key is safer to
    /// surface as "unknown" than to synthesize an outcome for.
    #[schema(value_type = Object, nullable = true)]
    pub reload: Option<reload::ConnectionOutcome>,
}

/// Response body for `POST .../credentials/test`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TestResponse {
    /// Whether a check was actually attempted — `false` for FIX.
    pub tested: bool,
    /// Only meaningful when `tested` is `true`: whether it passed.
    pub ok: bool,
    /// Present on anything short of a clean pass: the broker's rejection
    /// reason, or why nothing was attempted. Never derived from submitted
    /// input — see `TestOutcome`'s own doc comment.
    pub message: Option<String>,
}

/// Maps a `TestOutcome` onto the wire shape both the `test` endpoint and the
/// save path's `tested` flag draw from, so the two cannot disagree about
/// what counts as tested.
fn test_response(outcome: &TestOutcome) -> TestResponse {
    match outcome {
        TestOutcome::Passed => TestResponse { tested: true, ok: true, message: None },
        TestOutcome::Failed(msg) => TestResponse { tested: true, ok: false, message: Some(msg.clone()) },
        TestOutcome::NotTestable(why) => TestResponse { tested: false, ok: false, message: Some((*why).to_string()) },
    }
}

/// Whether `outcome` permits the write to proceed, as `Ok(())` / `Err(reason)`
/// so the 422 body can reuse the broker's own message with no reformatting.
/// `NotTestable` permits the write — see `credentials_api::test_broker` for
/// why "we didn't check" is not the same as "it failed" — only `Failed` does
/// not.
fn persist_gate(outcome: &TestOutcome) -> Result<(), String> {
    match outcome {
        TestOutcome::Passed | TestOutcome::NotTestable(_) => Ok(()),
        TestOutcome::Failed(msg) => Err(msg.clone()),
    }
}

/// A failed test must stop before the write. This is the gate the whole
/// feature rests on: a credential that cannot authenticate must never
/// replace one that can.
fn should_persist(result: &Result<(), String>) -> bool {
    result.is_ok()
}

/// The environment `credentials_api::test_broker` checks an Alpaca credential
/// against: the connection's *own* stored environment, never a hardcoded
/// literal. `AlpacaAdapter::new` picks the live endpoint only for exactly
/// `"LIVE"`, so testing a real `LIVE` credential against a hardcoded
/// `"PAPER"` 401s and the save gate refuses a credential that is perfectly
/// valid — the bug this function exists to make impossible to reintroduce.
/// `None` is never expected for a broker row (`load_brokers` always sets
/// `Some(env)`), so it falls back to the safer default, PAPER.
fn test_environment(conn_environment: Option<&str>) -> &str {
    conn_environment.unwrap_or("PAPER")
}

/// Parses the raw request body into a `CredentialSubmission`, deliberately
/// not `Json<CredentialSubmission>` at the extractor: axum's own extractor
/// answers a type mismatch (e.g. a numeric `port`) with its own 422 before
/// the handler ever runs — indistinguishable from this endpoint's
/// test-failed 422, which its docs promise means "did not authenticate".
/// That axum rejection also echoes the offending value into its message
/// ("invalid type: integer `12345`"). Taking a `Value` at the extractor and
/// parsing it here keeps a malformed body a 400 with a message that names
/// the problem, never the value.
fn parse_submission(raw: serde_json::Value) -> Result<CredentialSubmission, AdminError> {
    serde_json::from_value(raw).map_err(|_| AdminError {
        status: StatusCode::BAD_REQUEST,
        message: "request body could not be parsed: every field value must be a string".into(),
    })
}

/// Save (create or edit) a broker connection's credential.
///
/// The order is the requirement: decrypt what is already stored, merge the
/// submission over it (`credentials_api::parse_broker`), test the *merged*
/// result — never the raw submission, since an omitted field must inherit a
/// value that already passes — and only once that clears does anything get
/// written. A `Failed` test returns 422 with the broker's own message and
/// changes nothing: no write, no reload. `NotTestable` (FIX) proceeds to
/// save — `tested: false` in the response says so, rather than implying a
/// pass never earned.
///
/// The save itself reloads the running registry so the new credential takes
/// effect (or the cockpit is told a restart is needed) without a second
/// call — see `SaveResponse::reload`.
#[utoipa::path(
    put, path = "/admin/broker-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    request_body = CredentialSubmission,
    responses(
        (status = 200, description = "Saved, tested where possible, and reloaded", body = SaveResponse),
        (status = 400, description = "The request body was malformed, or could not be parsed into a credential (unknown field, bad port, ...)"),
        (status = 404, description = "Not found"),
        (status = 422, description = "The credential did not authenticate; nothing was written"),
        (status = 500, description = "The credential store could not be read, the master key is invalid or unset, or the reload failed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn put_broker_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(raw): Json<serde_json::Value>,
) -> Result<Json<SaveResponse>, AdminError> {
    info!(broker_connection_code = %code, "admin save broker connection credentials");

    let submission = parse_submission(raw)?;

    // A write, unlike a read, cannot tolerate an absent key: there would be
    // nothing to seal the credential under.
    let master = resolve_master_key()?.ok_or_else(|| AdminError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "no master key is configured (set oms.master_key in oms.toml); cannot save credentials".into(),
    })?;

    // Decrypt existing.
    let connections = crate::credentials::load_brokers(state.pool(), Some(&master))
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("broker_connection"))?;
    let existing = match &conn.credentials {
        CredentialState::Configured(c) => Some(c),
        CredentialState::Unconfigured | CredentialState::Error(_) => None,
    };
    // Captured before `conn.kind`/`existing` are consumed below — the
    // connection's own environment, never a hardcoded literal. See
    // `test_environment`'s doc comment for why this matters.
    let environment = test_environment(conn.environment.as_deref()).to_string();

    // Parse merged.
    let parsed = credentials_api::parse_broker(&conn.kind, existing, &submission)
        .map_err(|e| AdminError { status: StatusCode::BAD_REQUEST, message: e.to_string() })?;

    // Test.
    let outcome = credentials_api::test_broker(&parsed, &environment).await;
    let TestResponse { tested, message, .. } = test_response(&outcome);
    let gate = persist_gate(&outcome);
    if !should_persist(&gate) {
        return Err(AdminError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: gate.expect_err("should_persist(&gate) was false, so gate must be Err"),
        });
    }

    // Persist.
    crate::credentials::save_broker(state.pool(), &master, &code, &parsed)
        .await
        .map_err(map_db_error)?;

    // Reload — the existing, serialized, FIX-aware path; not a second one.
    let report = reload_with_key(state.clone(), Some(master.clone())).await?;
    let reload = report.0.connections.into_iter().find(|(c, _)| *c == code).map(|(_, outcome)| outcome);

    let refreshed = crate::credentials::load_brokers(state.pool(), Some(&master))
        .await
        .map_err(map_db_error)?;
    let conn = refreshed
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("broker_connection"))?;

    Ok(Json(SaveResponse { redacted: redact_connection(conn), tested, message, reload }))
}

/// Clear a broker connection's stored credential.
///
/// Also reloads: leaving a removed credential's adapter live would be the
/// silent-mismatch class this project has hit repeatedly (see `reload.rs`'s
/// module doc). The reload's own outcome is not carried in the response —
/// unlike the save path, there is nothing to test or report other than
/// "unconfigured", which `redacted.state` already says.
#[utoipa::path(
    delete, path = "/admin/broker-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 200, description = "Cleared and reloaded — now unconfigured", body = RedactedCredentials),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, the master key is invalid, or the reload failed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_broker_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<RedactedCredentials>, AdminError> {
    info!(broker_connection_code = %code, "admin delete broker connection credentials");

    // Cleared before the reload runs: if the reload below fails, this row is
    // already null while the old adapter keeps routing on the credential it
    // was built with. That is a real gap, but a self-healing one — the
    // `UPDATE` still reports a row affected on an already-null row (it
    // matches on `code`, not on `credentials IS NOT NULL`), so a retried
    // `DELETE` runs the reload again with nothing left to clear. Clearing
    // *after* a successful reload would remove that safety net: a failed
    // reload would then leave the credential live with no record that
    // deletion was ever requested, and a retry would have nothing to update
    // to notice the difference. Don't reorder this without keeping that
    // property.
    let result = sqlx::query(
        "UPDATE broker_connection \
         SET credentials = NULL, credentials_updated_at = NULL, updated_at = now() \
         WHERE code = $1",
    )
    .bind(&code)
    .execute(state.pool())
    .await
    .map_err(map_db_error)?;
    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("broker_connection"));
    }

    // Disarm the adapter before reporting success — the reload path, not a
    // second one. The report itself is not surfaced here (see this fn's doc
    // comment); only that the reload ran before we report success.
    let master = resolve_master_key()?;
    let _report = reload_with_key(state.clone(), master.clone()).await?;

    let connections = crate::credentials::load_brokers(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("broker_connection"))?;

    Ok(Json(redact_connection(conn)))
}

/// Test the credential already stored for a broker connection, changing
/// nothing. Distinct from the save path's pre-write test: this one exists so
/// an operator can re-check a credential that is already live (e.g. after a
/// broker-side key rotation) without resubmitting it.
#[utoipa::path(
    post, path = "/admin/broker-connections/{code}/credentials/test", tag = "admin",
    params(("code" = String, Path, description = "Broker connection code")),
    responses(
        (status = 200, description = "Test outcome for the stored credential", body = TestResponse),
        (status = 400, description = "No credential is stored for this connection"),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, or the stored credential could not be decrypted"),
    ),
    security(("bearer_token" = []))
)]
pub async fn test_broker_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<TestResponse>, AdminError> {
    info!(broker_connection_code = %code, "admin test broker connection credentials");

    let master = resolve_master_key()?;
    let connections = crate::credentials::load_brokers(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("broker_connection"))?;
    let environment = test_environment(conn.environment.as_deref()).to_string();

    let creds = match conn.credentials {
        CredentialState::Configured(c) => c,
        CredentialState::Unconfigured => {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: "no credential is stored for this connection".into(),
            });
        }
        CredentialState::Error(e) => {
            return Err(AdminError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("stored credential could not be decrypted: {e}"),
            });
        }
    };

    let outcome = credentials_api::test_broker(&creds, &environment).await;
    Ok(Json(test_response(&outcome)))
}

/// Re-read the credential store and apply what can be applied without a
/// process restart.
///
/// Returns 200 even when some connections could not be reloaded: a FIX
/// credential (IBKR, or Binance running FIX) always reports `RestartRequired`
/// — that is the expected outcome, not a failure — and a non-2xx would make
/// the cockpit treat normal operation as an error. The per-connection
/// outcomes in the body carry the detail, never the credential itself.
///
/// Reports 500 and leaves the running registry exactly as it was, without
/// swapping, in three cases: a database error reading either store; a master
/// key that is configured but invalid; or — the case `boot` itself already
/// refuses to start in, via the same `nothing_decrypted` predicate — every
/// stored credential across *both* stores failing to decrypt under the
/// resolved key while nothing at all decoded. That last case is this
/// endpoint's own headline scenario: `oms config rotate-key` re-seals every
/// row from a separate process while this server still holds the old key in
/// memory (`config::load` is memoized, so an edit to `oms.toml` after boot is
/// never picked up by this endpoint either — restart to pick up a changed
/// master key itself). POSTing a reload right after a rotation, instead of
/// restarting, is exactly the workflow this endpoint exists to avoid a
/// restart for; without this gate it would swap in a registry with every
/// broker unregistered and still report 200.
#[utoipa::path(
    post, path = "/admin/connections/reload", tag = "admin",
    responses(
        (status = 200, description = "Per-connection reload report (codes and outcomes only — no credential material)"),
        (status = 500, description = "The credential store could not be read, the master key is invalid, or nothing at all decrypted; nothing was changed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn reload_connections(
    State(state): State<AppState>,
) -> Result<Json<reload::ReloadReport>, AdminError> {
    let master = resolve_master_key()?;

    reload_with_key(state, master).await
}

/// Own the operation independently of the HTTP request: once stream shutdown
/// starts, a disconnected client must not cancel the reload halfway through.
/// The explicit key also lets integration tests exercise the real path without
/// reading the user's memoized configuration or mutating process environment.
pub(crate) async fn reload_with_key(
    state: AppState,
    master: Option<crate::secrets::MasterKey>,
) -> Result<Json<reload::ReloadReport>, AdminError> {
    reload_using(state, master, reload::LiveReloadStreams).await
}

pub(crate) async fn reload_using(
    state: AppState,
    master: Option<crate::secrets::MasterKey>,
    streams: impl reload::ReloadStreams,
) -> Result<Json<reload::ReloadReport>, AdminError> {
    tokio::spawn(async move { apply_reload(state, master, streams).await })
        .await
        .map_err(|_| AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "reload task failed; inspect server logs and connection health".into(),
        })?
}

async fn apply_reload(
    state: AppState,
    master: Option<crate::secrets::MasterKey>,
    streams: impl reload::ReloadStreams,
) -> Result<Json<reload::ReloadReport>, AdminError> {
    let _reload_guard = state.lock_reload().await;

    // Both stores are read before anything is touched: a failure in either one
    // must abort the whole reload rather than swap in a registry built from a
    // half-read store. One snapshot also prevents a concurrent key rotation
    // from being observed between the broker and feed queries.
    let mut snapshot = state.pool().begin().await.map_err(reload_store_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *snapshot).await.map_err(reload_store_error)?;
    let broker_connections = crate::credentials::load_brokers(&mut *snapshot, master.as_ref())
        .await
        .map_err(|e| AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to load broker connections: {e}"),
        })?;
    let feed_connections = crate::credentials::load_feeds(&mut *snapshot, master.as_ref())
        .await
        .map_err(|e| AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to load feed connections: {e}"),
        })?;
    snapshot.commit().await.map_err(reload_store_error)?;

    // Mirror boot's own refusal gate (`crate::nothing_decrypted`) before
    // touching anything: `credentials::decode` turns an unreadable blob into
    // `CredentialState::Error`, not an `Err`, so the two loads above succeed
    // even when the resolved key opens nothing at all. Without this check a
    // reload that raced a `rotate-key` run in a separate process — this
    // endpoint's own headline use case — would swap in a registry with every
    // broker unregistered and still report 200. See this function's doc
    // comment for why this is not merely a defensive check.
    if must_refuse_reload(&broker_connections, &feed_connections) {
        return Err(AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "refusing to reload: credentials are stored but none of them could be \
                      decrypted under the configured master key; the running registry is unchanged"
                .to_string(),
        });
    }

    info!("admin reload connections");

    let deps = reload::RegistrationDeps {
        pool: state.pool().clone(),
        stream_health: state.stream_health().clone(),
        kafka: state.kafka().cloned(),
        position_changed_tx: state.position_changed_tx(),
    };

    // Release this snapshot after registration, before joining old streams.
    // The reload lock keeps another writer from changing it during this pass.
    let current = state.registry();
    let reload::RegistrationOutput {
        registry,
        mut report,
        alpaca_creds,
        // Always empty on a reload: every active Binance connection classifies
        // `RestartRequired` here (see `classify`'s doc comment), so
        // `build_registry` never takes the "build a fresh REST adapter" branch
        // outside of boot. Nothing to spawn a stream for.
        binance_rest_adapters: _binance_rest_adapters,
    } = reload::build_registry(&broker_connections, false, &current, &deps).await;
    drop(current);

    state.swap_registry(registry);

    // `build_registry` pushes exactly one report entry per input connection,
    // in order (verified by reading every branch), which is what makes the
    // positional zip below sound. This is a real coupling across a module
    // boundary rather than something the type system enforces, so pin it.
    //
    // `assert_eq!`, not `debug_assert_eq!`: a release build is exactly where this
    // must not silently pass. `zip` truncates on a length mismatch and — worse —
    // shifts the pairing, so a broken invariant would apply one connection's
    // outcome to another connection's environment and stop or restart the wrong
    // execution stream. Failing loudly beats routing fills through the wrong one.
    assert_eq!(
        broker_connections.len(),
        report.connections.len(),
        "build_registry must report exactly one outcome per input connection, in order"
    );

    // For each broker connection: restart its execution stream if this pass
    // re-registered it, or stop one still running under a credential that is
    // no longer live if this pass did not.
    //
    // `restarts_execution_stream` is true only for `Registered`, and
    // `classify` never reports `Registered` for a FIX connection on reload
    // (see its own doc comment) — so the restart arm only ever fires for
    // Alpaca. Using it here, rather than re-deriving the same answer from
    // `alpaca_creds` membership, is what makes this loop follow the outcome
    // the report already committed to, instead of a second, potentially
    // diverging judgment call.
    //
    // The stop arm covers `Disabled` and `Unconfigured`, and a `Failed` that
    // truly has nothing running: nothing before this task ever stopped the
    // *execution stream* for one of them — an operator disabling
    // `alpaca-paper` because a key leaked would see the report say `Disabled`
    // while the trade-update websocket kept ingesting fills on the old
    // credential. `RestartRequired` is deliberately excluded: its adapter is
    // carried forward and is still meant to be running.
    //
    // The condition is "no Alpaca adapter survived this pass for this
    // environment", not "the outcome wasn't Registered" — those differ for
    // exactly one case: an `Error`ed Alpaca connection whose credential
    // failed to decrypt but whose adapter was carried forward from `current`
    // (see `build_registry`'s `Error` arm, added for the same "one bad row
    // must not disarm a working connection" reason `RestartRequired` already
    // gets). Aborting the stream there, on the outcome alone, would stop
    // fills while orders kept routing through the surviving adapter —
    // recreating, in miniature, the exact order/fill split Task 4 closed for
    // the double-stream case, just from the opposite direction (zero streams
    // instead of two). `alpaca_exec_stream_code` is keyed by environment, not
    // by broker kind, so checking `get_alpaca` from a non-Alpaca connection's
    // iteration (e.g. an IBKR row sharing the same environment) is always a
    // harmless no-op — nothing is ever registered under that key for them.
    for (conn, (_, outcome)) in broker_connections.iter().zip(report.connections.iter()) {
        let env_name: &'static str = match conn.environment.as_deref() {
            Some("PAPER") => "PAPER",
            Some("LIVE") => "LIVE",
            _ => continue,
        };
        let alpaca_adapter = state.registry().get_alpaca(env_name);
        if reload::restarts_execution_stream(outcome) {
            if let (Some((key, secret)), Some(adapter)) = (alpaca_creds.get(env_name), alpaca_adapter) {
                streams.alpaca(env_name, (key.clone(), secret.clone()), adapter, &deps, state.streams()).await;
            }
        } else if should_stop_alpaca_stream(outcome, alpaca_adapter.is_some()) {
            stop_execution_stream(&state, "ALPACA", env_name, &reload::alpaca_exec_stream_code(env_name)).await;
        }
    }

    // Reconcile known execution tasks against the resulting registry, not just
    // input rows: a deleted row has no iteration in the loop above. Binance REST
    // is restart-required but its existing task must still stop when disabled.
    for env_name in ["PAPER", "LIVE"] {
        if state.registry().get_alpaca(env_name).is_none() {
            stop_execution_stream(&state, "ALPACA", env_name, &reload::alpaca_exec_stream_code(env_name)).await;
        }
        if state.registry().get("BINANCE", env_name).is_none() {
            stop_execution_stream(&state, "BINANCE", env_name, &reload::binance_exec_stream_code(env_name)).await;
        }
    }

    // Feed connections have no FIX-shaped "cannot reload" case: Databento is a
    // plain REST/WS credential behind a supervised task, restartable the same
    // way an Alpaca adapter is. Classified and appended to the same report so
    // the cockpit sees every connection — broker or feed — in one response.
    //
    // A feed classified `Disabled` or `Unconfigured` — a deliberate operator
    // state, not a storage failure — has its task stopped too:
    // `restart_databento_feed` (in `reload.rs`) aborts and re-inserts under the
    // hardcoded literal `"databento-opra"`, *not* `conn.code` — there being
    // only one Databento feed made a bare literal simplest at the time. That
    // only lines up with `abort_and_remove(&conn.code)` below because
    // `classify_feed` pins its `Registered` case to that exact code (see
    // there); a second feed connection would need `restart_databento_feed`
    // keyed by `conn.code` for real, not this coincidence. Otherwise a feed an
    // operator just turned off keeps silently streaming quotes into
    // `MarkStore` on its old credential — the same class of bug as the Alpaca
    // case above.
    //
    // `Failed` (a decrypt error) is deliberately NOT stopped — the mirror of
    // the broker loop's `Error`/`RestartRequired` carry-forward above, applied
    // to a feed: the running task was built from a credential that decrypted
    // fine the last time this ran, and a *stored* row becoming unreadable is
    // not evidence the feed itself stopped working. Unlike the broker case
    // there is nothing to carry forward — the task was never touched, so
    // simply not calling `abort_and_remove` is all "carry forward" means
    // here. A stopped feed means no marks, which means positions go
    // unpriced — worse than leaving it on its last-known-good credential
    // until the row is fixed. The report still says `Failed`, so the
    // operator is told even though the feed keeps running.
    for conn in &feed_connections {
        let outcome = classify_feed(conn);
        match (&outcome, &conn.credentials) {
            (reload::ConnectionOutcome::Registered, CredentialState::Configured(FeedCredentials::Databento { api_key })) => {
                streams.databento(api_key.clone(), &state).await;
            }
            _ if should_stop_databento_feed(&outcome) && conn.code == "databento-opra" => stop_databento_feed(&state).await,
            _ => {}
        }
        report.connections.push((conn.code.clone(), outcome));
    }
    if !feed_connections.iter().any(|conn| conn.code == "databento-opra") {
        stop_databento_feed(&state).await;
    }

    Ok(Json(report))
}

fn reload_store_error(error: sqlx::Error) -> AdminError {
    AdminError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to read credential snapshot: {error}"),
    }
}

async fn stop_execution_stream(state: &AppState, broker: &str, environment: &str, code: &str) {
    if state.streams().abort_and_remove(code).await {
        state.stream_health().handle(broker, environment, crate::stream_health::StreamKind::Execution)
            .set_down("connection disabled, unconfigured, or removed");
    }
}

async fn stop_databento_feed(state: &AppState) {
    if state.streams().abort_and_remove("databento-opra").await {
        state.stream_health().handle("DATABENTO", "OPRA", crate::stream_health::StreamKind::Feed)
            .set_down("connection disabled, unconfigured, or removed");
    }
    state.doorbells().remove("databento-opra");
}

/// Whether `reload_connections` must refuse rather than swap: the same
/// judgment `crate::nothing_decrypted` makes at boot, applied to what this
/// reload just read. Combines *both* stores into the one configured/error
/// verdict — a broker-only or feed-only reading of "did anything decrypt"
/// would miss the case where, say, every broker credential is readable but
/// every feed credential just got re-sealed under a key this process does not
/// have yet (or vice versa); either way, if literally nothing across the
/// whole store decoded while something is known to be stored and unusable,
/// this reload has nothing safe to swap in.
fn must_refuse_reload(
    broker_connections: &[Connection<BrokerCredentials>],
    feed_connections: &[Connection<FeedCredentials>],
) -> bool {
    let any_configured = broker_connections.iter().any(|c| matches!(c.credentials, CredentialState::Configured(_)))
        || feed_connections.iter().any(|c| matches!(c.credentials, CredentialState::Configured(_)));
    let any_error = broker_connections.iter().any(|c| matches!(c.credentials, CredentialState::Error(_)))
        || feed_connections.iter().any(|c| matches!(c.credentials, CredentialState::Error(_)));
    crate::nothing_decrypted(any_configured, any_error)
}

/// Whether the reload loop should stop an Alpaca environment's execution
/// stream: true iff this pass did not just restart it (`outcome` is not
/// `Registered`) *and* nothing survived the swap to keep serving orders under
/// it either.
///
/// The second half is what keeps this different from a plain "outcome wasn't
/// `Registered`" check: an `Error`ed connection whose credential failed to
/// decrypt can still have `alpaca_adapter_survived = true`, because
/// `build_registry`'s `Error` arm carries the previous adapter forward (the
/// same "one bad row must not disarm a working connection" reasoning
/// `RestartRequired` already gets). Stopping the stream in that case would
/// halt fills while orders kept routing through the surviving adapter —
/// recreating, from the opposite direction, the order/fill split Task 4
/// closed for the double-stream case.
fn should_stop_alpaca_stream(outcome: &reload::ConnectionOutcome, alpaca_adapter_survived: bool) -> bool {
    !reload::restarts_execution_stream(outcome) && !alpaca_adapter_survived
}

/// Classify one feed connection for the reload report — the feed-side
/// counterpart to `reload::classify`. Simpler than the broker version: no
/// feed credential owns a thread with no stop path, so there is no
/// `RestartRequired` case here — a configured, active feed either applies
/// immediately or is reported `Failed`.
fn classify_feed(conn: &Connection<FeedCredentials>) -> reload::ConnectionOutcome {
    if conn.status != "ACTIVE" {
        return reload::ConnectionOutcome::Disabled;
    }
    match &conn.credentials {
        CredentialState::Unconfigured => reload::ConnectionOutcome::Unconfigured,
        CredentialState::Error(e) => reload::ConnectionOutcome::Failed(e.clone()),
        // Only one Databento feed is implemented (the OPRA feed, keyed
        // "databento-opra" — see the matching guard boot itself runs in
        // `serve()`); a different code with the same credential shape is a
        // misconfiguration, not something to start.
        CredentialState::Configured(FeedCredentials::Databento { .. }) if conn.code == "databento-opra" => {
            reload::ConnectionOutcome::Registered
        }
        CredentialState::Configured(FeedCredentials::Databento { .. }) => {
            reload::ConnectionOutcome::Failed("unsupported Databento feed connection".into())
        }
    }
}

/// Whether the reload loop should stop the Databento feed task: true only for
/// `Disabled` and `Unconfigured` — deliberate operator states. `Registered` is
/// handled by `restart_databento_feed` itself; `Failed` is excluded on
/// purpose, the feed-side mirror of `should_stop_alpaca_stream`'s own
/// asymmetry for brokers: a decrypt failure on a *stored* row is not evidence
/// the feed itself stopped working, and a stopped feed means no marks, which
/// means positions go unpriced. The task is never touched in that case, so
/// "carry forward" here needs no extra step beyond not calling this.
fn should_stop_databento_feed(outcome: &reload::ConnectionOutcome) -> bool {
    matches!(outcome, reload::ConnectionOutcome::Disabled | reload::ConnectionOutcome::Unconfigured)
}

// ── Feed connections ──────────────────────────────────────────────────────────
//
// Mirrors the "Broker connections" section above: same redacted view, same
// write gate (decrypt existing → parse merged → test → persist → reload), same
// 400-vs-422 split. The pieces that don't vary by credential kind —
// `resolve_master_key`, `redact_connection`, `test_response`, `persist_gate`,
// `should_persist`, `parse_submission`, `reload_with_key` — are reused as-is
// from that section, not copied.

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct FeedConnectionSummary {
    pub code: String,
    pub provider: String,
    /// Nullable in the schema — a feed connection can exist before its
    /// dataset is known, same as credentials can be unconfigured.
    pub dataset: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The configured feed connections themselves (rows), distinct from
/// `list_feeds` above, which reports the ranked market-data source *policy*
/// (`provider_feed_policy`) plus coverage counts. Nothing served
/// `feed_connection` rows before this — the cockpit's feed credentials page
/// has no list to render without it.
#[utoipa::path(
    get, path = "/admin/feed-connections", tag = "admin",
    responses((status = 200, description = "OK", body = [FeedConnectionSummary])),
    security(("bearer_token" = []))
)]
pub async fn list_feed_connections(
    State(state): State<AppState>,
) -> Result<Json<Vec<FeedConnectionSummary>>, AdminError> {
    info!("admin list feed connections");
    let records = sqlx::query_as::<_, FeedConnectionSummary>(
        r#"
        SELECT code, provider, dataset, status, created_at, updated_at
        FROM feed_connection
        ORDER BY code
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

/// What is configured for one feed connection, with every secret withheld.
/// See `get_broker_connection_credentials` — identical shape and `state`
/// semantics, just backed by `load_feeds` instead of `load_brokers`.
#[utoipa::path(
    get, path = "/admin/feed-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Feed connection code")),
    responses(
        (status = 200, description = "OK — configured, unconfigured, or error; never a decrypted secret", body = RedactedCredentials),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, or the configured master key is invalid"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_feed_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<RedactedCredentials>, AdminError> {
    info!(feed_connection_code = %code, "admin get feed connection credentials");

    let master = resolve_master_key()?;

    let connections = crate::credentials::load_feeds(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;

    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("feed_connection"))?;

    Ok(Json(redact_connection(conn)))
}

/// Save (create or edit) a feed connection's credential. See
/// `put_broker_connection_credentials` for the order this follows and why —
/// the only differences are `parse_feed`/`test_feed` in place of
/// `parse_broker`/`test_broker` (feeds have no per-connection environment to
/// thread through) and `save_feed` in place of `save_broker`.
#[utoipa::path(
    put, path = "/admin/feed-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Feed connection code")),
    request_body = CredentialSubmission,
    responses(
        (status = 200, description = "Saved, tested where possible, and reloaded", body = SaveResponse),
        (status = 400, description = "The request body was malformed, or could not be parsed into a credential (unknown field, ...)"),
        (status = 404, description = "Not found"),
        (status = 422, description = "The credential did not authenticate; nothing was written"),
        (status = 500, description = "The credential store could not be read, the master key is invalid or unset, or the reload failed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn put_feed_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(raw): Json<serde_json::Value>,
) -> Result<Json<SaveResponse>, AdminError> {
    info!(feed_connection_code = %code, "admin save feed connection credentials");

    let submission = parse_submission(raw)?;

    // A write, unlike a read, cannot tolerate an absent key — same as brokers.
    let master = resolve_master_key()?.ok_or_else(|| AdminError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "no master key is configured (set oms.master_key in oms.toml); cannot save credentials".into(),
    })?;

    // Decrypt existing. 404s here even though `save_feed` itself would
    // upsert the row (see its doc comment) — this endpoint only edits a feed
    // connection that already exists, same as the broker path.
    let connections = crate::credentials::load_feeds(state.pool(), Some(&master))
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("feed_connection"))?;
    let existing = match &conn.credentials {
        CredentialState::Configured(c) => Some(c),
        CredentialState::Unconfigured | CredentialState::Error(_) => None,
    };

    // Parse merged.
    let parsed = credentials_api::parse_feed(&conn.kind, existing, &submission)
        .map_err(|e| AdminError { status: StatusCode::BAD_REQUEST, message: e.to_string() })?;

    // Test.
    let outcome = credentials_api::test_feed(&parsed).await;
    let TestResponse { tested, message, .. } = test_response(&outcome);
    let gate = persist_gate(&outcome);
    if !should_persist(&gate) {
        return Err(AdminError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: gate.expect_err("should_persist(&gate) was false, so gate must be Err"),
        });
    }

    // Persist.
    crate::credentials::save_feed(state.pool(), &master, &code, &parsed)
        .await
        .map_err(map_db_error)?;

    // Reload — the existing, serialized path; not a second one.
    let report = reload_with_key(state.clone(), Some(master.clone())).await?;
    let reload = report.0.connections.into_iter().find(|(c, _)| *c == code).map(|(_, outcome)| outcome);

    let refreshed = crate::credentials::load_feeds(state.pool(), Some(&master))
        .await
        .map_err(map_db_error)?;
    let conn = refreshed
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("feed_connection"))?;

    Ok(Json(SaveResponse { redacted: redact_connection(conn), tested, message, reload }))
}

/// Clear a feed connection's stored credential. See
/// `delete_broker_connection_credentials` for the ordering rationale (clear
/// before reload, so a failed reload still leaves a retryable state).
#[utoipa::path(
    delete, path = "/admin/feed-connections/{code}/credentials", tag = "admin",
    params(("code" = String, Path, description = "Feed connection code")),
    responses(
        (status = 200, description = "Cleared and reloaded — now unconfigured", body = RedactedCredentials),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, the master key is invalid, or the reload failed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_feed_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<RedactedCredentials>, AdminError> {
    info!(feed_connection_code = %code, "admin delete feed connection credentials");

    let result = sqlx::query(
        "UPDATE feed_connection \
         SET credentials = NULL, credentials_updated_at = NULL, updated_at = now() \
         WHERE code = $1",
    )
    .bind(&code)
    .execute(state.pool())
    .await
    .map_err(map_db_error)?;
    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("feed_connection"));
    }

    // Disarm the feed task before reporting success — same as the broker path.
    let master = resolve_master_key()?;
    let _report = reload_with_key(state.clone(), master.clone()).await?;

    let connections = crate::credentials::load_feeds(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("feed_connection"))?;

    Ok(Json(redact_connection(conn)))
}

/// Test the credential already stored for a feed connection, changing
/// nothing. See `test_broker_connection_credentials` — identical shape,
/// backed by `load_feeds`/`test_feed` instead of `load_brokers`/`test_broker`.
/// Unlike FIX, a stored Databento key genuinely gets tested here: `test_feed`
/// runs the same connect-and-auth handshake as the save path's pre-write
/// check, so this can return a real pass or fail rather than `NotTestable`.
#[utoipa::path(
    post, path = "/admin/feed-connections/{code}/credentials/test", tag = "admin",
    params(("code" = String, Path, description = "Feed connection code")),
    responses(
        (status = 200, description = "Test outcome for the stored credential", body = TestResponse),
        (status = 400, description = "No credential is stored for this connection"),
        (status = 404, description = "Not found"),
        (status = 500, description = "The credential store could not be read, or the stored credential could not be decrypted"),
    ),
    security(("bearer_token" = []))
)]
pub async fn test_feed_connection_credentials(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<TestResponse>, AdminError> {
    info!(feed_connection_code = %code, "admin test feed connection credentials");

    let master = resolve_master_key()?;
    let connections = crate::credentials::load_feeds(state.pool(), master.as_ref())
        .await
        .map_err(map_db_error)?;
    let conn = connections
        .into_iter()
        .find(|c| c.code == code)
        .ok_or_else(|| AdminError::not_found("feed_connection"))?;

    let creds = match conn.credentials {
        CredentialState::Configured(c) => c,
        CredentialState::Unconfigured => {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: "no credential is stored for this connection".into(),
            });
        }
        CredentialState::Error(e) => {
            return Err(AdminError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("stored credential could not be decrypted: {e}"),
            });
        }
    };

    let outcome = credentials_api::test_feed(&creds).await;
    Ok(Json(test_response(&outcome)))
}

// ── API key management ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateKey {
    pub name: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ApiKeyRecord {
    pub id: Uuid,
    pub principal_id: Uuid,
    pub key_id: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

#[utoipa::path(
    get, path = "/admin/principals/{id}/keys", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    responses(
        (status = 200, description = "OK", body = [ApiKeyRecord]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_principal_keys(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
) -> Result<Json<Vec<ApiKeyRecord>>, AdminError> {
    info!(principal_id = %principal_id, "admin list keys");
    let records = sqlx::query_as::<_, ApiKeyRecord>(
        r#"
        SELECT id, principal_id, key_id, name, created_at
        FROM api_key
        WHERE principal_id = $1 AND revoked_at IS NULL
        ORDER BY created_at DESC
        "#,
    )
    .bind(principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    post, path = "/admin/principals/{id}/keys", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    request_body = CreateKey,
    responses(
        (status = 200, description = "Created — plaintext secret included once", body = ApiKeyRecord),
    ),
    security(("bearer_token" = []))
)]
pub async fn register_principal_key(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
    Json(payload): Json<CreateKey>,
) -> Result<Json<ApiKeyRecord>, AdminError> {
    info!(principal_id = %principal_id, "admin register key");
    let KeyMaterial { key_id, plaintext_secret, secret_hash } = generate_key_material().await?;

    let mut record = sqlx::query_as::<_, ApiKeyRecord>(
        r#"
        INSERT INTO api_key (principal_id, key_id, secret_hash, name)
        VALUES ($1, $2, $3, $4)
        RETURNING id, principal_id, key_id, name, created_at
        "#,
    )
    .bind(principal_id)
    .bind(&key_id)
    .bind(secret_hash)
    .bind(payload.name)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    record.secret = Some(plaintext_secret);
    Ok(Json(record))
}

#[utoipa::path(
    delete, path = "/admin/principals/{id}/keys/{key_id}", tag = "admin",
    params(
        ("id" = Uuid, Path, description = "Principal ID"),
        ("key_id" = String, Path, description = "Key ID"),
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn revoke_principal_key(
    State(state): State<AppState>,
    Path((principal_id, key_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, AdminError> {
    info!(principal_id = %principal_id, key_id = %key_id, "admin revoke key");

    let result = sqlx::query(
        r#"
        UPDATE api_key SET revoked_at = now()
        WHERE key_id = $1 AND principal_id = $2 AND revoked_at IS NULL
        "#,
    )
    .bind(&key_id)
    .bind(principal_id)
    .execute(state.pool())
    .await
    .map_err(map_db_error)?;

    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("key"));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── Trading tokens ────────────────────────────────────────────────────────────
//
// A "trading token" is an ordinary `api_key` presented to the trading routes as a
// single bearer string `"{key_id}.{secret}"` (see `auth::extract_trading_credentials`).
// The endpoints here make it a one-click, ready-to-trade credential: generating one
// can auto-provision the principal + portfolio + `can_trade` grant it needs.

/// Freshly generated key material, before it's persisted.
struct KeyMaterial {
    key_id: String,
    plaintext_secret: String,
    secret_hash: String,
}

/// Generate a new `(key_id, secret)` and its bcrypt hash. `key_id` = `ak_<uuid>`,
/// secret = `sk_<base64url(32 bytes)>`. Hashing runs off the async pool.
async fn generate_key_material() -> Result<KeyMaterial, AdminError> {
    let key_id = format!("ak_{}", Uuid::new_v4().simple());
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    raw[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let plaintext_secret = format!("sk_{}", general_purpose::URL_SAFE_NO_PAD.encode(raw));

    let to_hash = plaintext_secret.clone();
    let secret_hash = tokio::task::spawn_blocking(move || bcrypt::hash(&to_hash, 12))
        .await
        .map_err(|_| AdminError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "hash task failed".to_string() })?
        .map_err(|_| AdminError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "failed to hash secret".to_string() })?;

    Ok(KeyMaterial { key_id, plaintext_secret, secret_hash })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateTradingToken {
    /// Mint the token under this existing principal (the trader/strategy/service it
    /// belongs to). Create principals on the Principals admin surface first.
    pub principal_id: Uuid,
    /// Optionally entitle the principal to trade this portfolio (adds a `can_trade`
    /// grant). Omit if the principal is already granted, or to grant separately.
    pub portfolio_id: Option<Uuid>,
    /// Human label for this key (shown in the token list). Optional.
    pub label: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TradingTokenCreated {
    /// The single bearer token — `Authorization: Bearer <token>`. Shown once.
    pub token: String,
    pub key_id: String,
    pub principal_id: Uuid,
    pub portfolio_id: Option<Uuid>,
    pub label: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct TradingTokenRow {
    pub key_id: String,
    pub label: Option<String>,
    pub principal_id: Uuid,
    pub principal_code: String,
    pub principal_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[utoipa::path(
    post, path = "/admin/trading-tokens", tag = "admin",
    request_body = CreateTradingToken,
    responses(
        (status = 200, description = "Created — single bearer token included once", body = TradingTokenCreated),
        (status = 422, description = "Auto-provision needs an active broker connection"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_trading_token(
    State(state): State<AppState>,
    Json(payload): Json<CreateTradingToken>,
) -> Result<Json<TradingTokenCreated>, AdminError> {
    let label = payload.label.clone();
    let principal_id = payload.principal_id;
    info!(%principal_id, portfolio_id = ?payload.portfolio_id, "admin create trading token");

    let material = generate_key_material().await?;
    let mut tx = state.pool().begin().await.map_err(map_db_error)?;

    // Optionally entitle the principal to trade a portfolio. (The token belongs to
    // this principal; its grants decide what the token can trade.)
    if let Some(portfolio_id) = payload.portfolio_id {
        sqlx::query(
            "INSERT INTO principal_portfolio_grant \
                (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
             VALUES ($1, $2, $3, true, true, false) \
             ON CONFLICT (principal_id, portfolio_id) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;
    }

    // Persist the api key under the principal.
    sqlx::query("INSERT INTO api_key (principal_id, key_id, secret_hash, name) VALUES ($1, $2, $3, $4)")
        .bind(principal_id)
        .bind(&material.key_id)
        .bind(&material.secret_hash)
        .bind(label.clone())
        .execute(&mut *tx)
        .await
        .map_err(map_db_error)?;

    tx.commit().await.map_err(map_db_error)?;

    Ok(Json(TradingTokenCreated {
        token: format!("{}.{}", material.key_id, material.plaintext_secret),
        key_id: material.key_id,
        principal_id,
        portfolio_id: payload.portfolio_id,
        label,
    }))
}

#[utoipa::path(
    get, path = "/admin/trading-tokens", tag = "admin",
    responses((status = 200, description = "Active trading tokens", body = [TradingTokenRow])),
    security(("bearer_token" = []))
)]
pub async fn list_trading_tokens(
    State(state): State<AppState>,
) -> Result<Json<Vec<TradingTokenRow>>, AdminError> {
    // Every api key is a trading credential (auth_middleware accepts it); list them
    // with the principal they belong to.
    let rows = sqlx::query_as::<_, TradingTokenRow>(
        "SELECT k.key_id, k.name AS label, k.principal_id, \
                p.code AS principal_code, p.display_name AS principal_name, k.created_at \
         FROM api_key k JOIN principal p ON p.id = k.principal_id \
         WHERE k.revoked_at IS NULL \
         ORDER BY k.created_at DESC",
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(rows))
}

#[utoipa::path(
    delete, path = "/admin/trading-tokens/{key_id}", tag = "admin",
    params(("key_id" = String, Path, description = "Token key id")),
    responses((status = 204, description = "Revoked"), (status = 404, description = "Not found")),
    security(("bearer_token" = []))
)]
pub async fn revoke_trading_token(
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<StatusCode, AdminError> {
    info!(key_id = %key_id, "admin revoke trading token");
    let result = sqlx::query("UPDATE api_key SET revoked_at = now() WHERE key_id = $1 AND revoked_at IS NULL")
        .bind(&key_id)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;
    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("token"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Session revocation ────────────────────────────────────────────────────────

/// How many of a principal's sessions were just revoked.
///
/// Not itself proof the principal has no *other* way in — a revoked session
/// only stops resolving on its next `lookup_session` (see
/// `sessions::lookup_session`'s `revoked_at IS NULL` filter); this endpoint
/// does not touch API keys.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RevokedSessions {
    pub revoked: u64,
}

/// Cut a principal off immediately: revoke every one of its live sessions.
///
/// Effective on the very next request from any of them — `lookup_session`
/// only resolves a session while `revoked_at IS NULL`, so there is nothing
/// to invalidate beyond the row itself (no cache sits in front of it).
/// Not mounted yet; wiring the route is a later task, so this (and
/// `RevokedSessions`) will show as dead code until then.
pub async fn revoke_principal_sessions(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
) -> Result<Json<RevokedSessions>, AdminError> {
    info!(principal_id = %principal_id, "admin revoke principal sessions");
    let revoked = crate::sessions::revoke_all_for_principal(state.pool(), principal_id)
        .await
        .map_err(map_db_error)?;
    Ok(Json(RevokedSessions { revoked }))
}

// ── Grant management ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateGrant {
    pub portfolio_id: Uuid,
    pub can_trade: bool,
    pub can_view: bool,
    pub can_allocate: bool,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateGrant {
    pub can_trade: Option<bool>,
    pub can_view: Option<bool>,
    pub can_allocate: Option<bool>,
}

#[utoipa::path(
    post, path = "/admin/principals/{id}/grants", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    request_body = CreateGrant,
    responses(
        (status = 200, description = "Created", body = Grant),
        (status = 409, description = "Grant already exists for this principal/portfolio"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_grant(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
    Json(payload): Json<CreateGrant>,
) -> Result<Json<Grant>, AdminError> {
    info!(principal_id = %principal_id, portfolio_id = %payload.portfolio_id, "admin create grant");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Grant>(
        r#"
        INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view, can_allocate)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, principal_id, portfolio_id, can_trade, can_view, can_allocate, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(principal_id)
    .bind(payload.portfolio_id)
    .bind(payload.can_trade)
    .bind(payload.can_view)
    .bind(payload.can_allocate)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/principals/{id}/grants", tag = "admin",
    params(("id" = Uuid, Path, description = "Principal ID")),
    responses(
        (status = 200, description = "OK", body = [Grant]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_grants(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
) -> Result<Json<Vec<Grant>>, AdminError> {
    info!(principal_id = %principal_id, "admin list grants");
    let records = sqlx::query_as::<_, Grant>(
        r#"
        SELECT id, principal_id, portfolio_id, can_trade, can_view, can_allocate, created_at, updated_at
        FROM principal_portfolio_grant
        WHERE principal_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(principal_id)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    patch, path = "/admin/principals/{id}/grants/{grant_id}", tag = "admin",
    params(
        ("id" = Uuid, Path, description = "Principal ID"),
        ("grant_id" = Uuid, Path, description = "Grant ID"),
    ),
    request_body = UpdateGrant,
    responses(
        (status = 200, description = "Updated", body = Grant),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_grant(
    State(state): State<AppState>,
    Path((principal_id, grant_id)): Path<(Uuid, Uuid)>,
    Json(payload): Json<UpdateGrant>,
) -> Result<Json<Grant>, AdminError> {
    info!(principal_id = %principal_id, grant_id = %grant_id, "admin update grant");
    let record = sqlx::query_as::<_, Grant>(
        r#"
        UPDATE principal_portfolio_grant
        SET
            can_trade    = COALESCE($1, can_trade),
            can_view     = COALESCE($2, can_view),
            can_allocate = COALESCE($3, can_allocate),
            updated_at   = now()
        WHERE id = $4 AND principal_id = $5
        RETURNING id, principal_id, portfolio_id, can_trade, can_view, can_allocate, created_at, updated_at
        "#,
    )
    .bind(payload.can_trade)
    .bind(payload.can_view)
    .bind(payload.can_allocate)
    .bind(grant_id)
    .bind(principal_id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("grant"))?;

    Ok(Json(record))
}

#[utoipa::path(
    delete, path = "/admin/principals/{id}/grants/{grant_id}", tag = "admin",
    params(
        ("id" = Uuid, Path, description = "Principal ID"),
        ("grant_id" = Uuid, Path, description = "Grant ID"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_grant(
    State(state): State<AppState>,
    Path((principal_id, grant_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, AdminError> {
    info!(principal_id = %principal_id, grant_id = %grant_id, "admin delete grant");
    let result = sqlx::query(
        "DELETE FROM principal_portfolio_grant WHERE id = $1 AND principal_id = $2",
    )
    .bind(grant_id)
    .bind(principal_id)
    .execute(state.pool())
    .await
    .map_err(map_db_error)?;

    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("grant"));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── Risk limits ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct RiskLimit {
    pub id: Uuid,
    pub portfolio_id: Uuid,
    pub instrument_id: String,
    pub trading_state: String,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateRiskLimit {
    pub portfolio_id: Uuid,
    pub instrument_id: String,
    pub trading_state: Option<String>,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateRiskLimit {
    pub trading_state: Option<String>,
    pub max_order_quantity: Option<f64>,
    pub max_order_notional: Option<f64>,
    pub max_position_quantity: Option<f64>,
    pub max_position_notional: Option<f64>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct RiskLimitFilter {
    pub portfolio_id: Option<Uuid>,
}

// NUMERIC columns are cast to double precision so FromRow decodes them into f64.
const RISK_LIMIT_SELECT: &str = "id, portfolio_id, instrument_id, trading_state, \
    max_order_quantity::double precision    AS max_order_quantity, \
    max_order_notional::double precision    AS max_order_notional, \
    max_position_quantity::double precision AS max_position_quantity, \
    max_position_notional::double precision AS max_position_notional, \
    created_at, updated_at";

fn validate_trading_state(s: &str) -> Result<(), AdminError> {
    match s {
        "ACTIVE" | "REDUCING" | "HALTED" => Ok(()),
        _ => Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: "trading_state must be ACTIVE, REDUCING, or HALTED".to_string(),
        }),
    }
}

#[utoipa::path(
    post, path = "/admin/risk-limits", tag = "admin",
    request_body = CreateRiskLimit,
    responses(
        (status = 200, description = "Created", body = RiskLimit),
        (status = 400, description = "Invalid trading_state"),
        (status = 409, description = "Limit already exists for this portfolio/instrument"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_risk_limit(
    State(state): State<AppState>,
    Json(payload): Json<CreateRiskLimit>,
) -> Result<Json<RiskLimit>, AdminError> {
    let trading_state = payload.trading_state.unwrap_or_else(|| "ACTIVE".to_string());
    validate_trading_state(&trading_state)?;
    info!(portfolio_id = %payload.portfolio_id, instrument_id = %payload.instrument_id, "admin create risk limit");
    let sql = format!(
        "INSERT INTO risk_limits \
           (portfolio_id, instrument_id, trading_state, max_order_quantity, \
            max_order_notional, max_position_quantity, max_position_notional) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING {RISK_LIMIT_SELECT}"
    );
    let record = sqlx::query_as::<_, RiskLimit>(&sql)
        .bind(payload.portfolio_id)
        .bind(payload.instrument_id)
        .bind(trading_state)
        .bind(payload.max_order_quantity)
        .bind(payload.max_order_notional)
        .bind(payload.max_position_quantity)
        .bind(payload.max_position_notional)
        .fetch_one(state.pool())
        .await
        .map_err(map_db_error)?;
    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/risk-limits", tag = "admin",
    params(RiskLimitFilter),
    responses((status = 200, description = "OK", body = [RiskLimit])),
    security(("bearer_token" = []))
)]
pub async fn list_risk_limits(
    State(state): State<AppState>,
    Query(filter): Query<RiskLimitFilter>,
) -> Result<Json<Vec<RiskLimit>>, AdminError> {
    info!("admin list risk limits");
    let sql = format!(
        "SELECT {RISK_LIMIT_SELECT} FROM risk_limits \
         WHERE ($1::uuid IS NULL OR portfolio_id = $1) ORDER BY created_at DESC"
    );
    let records = sqlx::query_as::<_, RiskLimit>(&sql)
        .bind(filter.portfolio_id)
        .fetch_all(state.pool())
        .await
        .map_err(map_db_error)?;
    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/risk-limits/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Risk limit ID")),
    responses(
        (status = 200, description = "OK", body = RiskLimit),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_risk_limit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<RiskLimit>, AdminError> {
    let sql = format!("SELECT {RISK_LIMIT_SELECT} FROM risk_limits WHERE id = $1");
    let record = sqlx::query_as::<_, RiskLimit>(&sql)
        .bind(id)
        .fetch_optional(state.pool())
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| AdminError::not_found("risk_limit"))?;
    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/risk-limits/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Risk limit ID")),
    request_body = UpdateRiskLimit,
    responses(
        (status = 200, description = "Updated", body = RiskLimit),
        (status = 400, description = "Invalid trading_state"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_risk_limit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateRiskLimit>,
) -> Result<Json<RiskLimit>, AdminError> {
    if let Some(ref ts) = payload.trading_state {
        validate_trading_state(ts)?;
    }
    info!(risk_limit_id = %id, "admin update risk limit");
    let sql = format!(
        "UPDATE risk_limits SET \
            trading_state         = COALESCE($1, trading_state), \
            max_order_quantity    = COALESCE($2, max_order_quantity), \
            max_order_notional    = COALESCE($3, max_order_notional), \
            max_position_quantity = COALESCE($4, max_position_quantity), \
            max_position_notional = COALESCE($5, max_position_notional), \
            updated_at = now() \
         WHERE id = $6 \
         RETURNING {RISK_LIMIT_SELECT}"
    );
    let record = sqlx::query_as::<_, RiskLimit>(&sql)
        .bind(payload.trading_state)
        .bind(payload.max_order_quantity)
        .bind(payload.max_order_notional)
        .bind(payload.max_position_quantity)
        .bind(payload.max_position_notional)
        .bind(id)
        .fetch_optional(state.pool())
        .await
        .map_err(map_db_error)?
        .ok_or_else(|| AdminError::not_found("risk_limit"))?;
    Ok(Json(record))
}

#[utoipa::path(
    delete, path = "/admin/risk-limits/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Risk limit ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_risk_limit(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AdminError> {
    info!(risk_limit_id = %id, "admin delete risk limit");
    let result = sqlx::query("DELETE FROM risk_limits WHERE id = $1")
        .bind(id)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;
    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("risk_limit"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Instruments (lookup for forms) ────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct InstrumentSummary {
    pub id: i64,
    pub symbol: String,
    pub name: String,
    pub venue: String,
    pub asset_class: String,
    pub status: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct InstrumentSearch {
    pub search: Option<String>,
    pub limit: Option<i64>,
}

#[utoipa::path(
    get, path = "/admin/instruments", tag = "admin",
    params(InstrumentSearch),
    responses((status = 200, description = "OK", body = [InstrumentSummary])),
    security(("bearer_token" = []))
)]
pub async fn list_instruments(
    State(state): State<AppState>,
    Query(params): Query<InstrumentSearch>,
) -> Result<Json<Vec<InstrumentSummary>>, AdminError> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let pattern = params.search.as_deref().map(|s| format!("%{s}%"));
    let records = sqlx::query_as::<_, InstrumentSummary>(
        "SELECT id, symbol, name, venue, asset_class, status \
         FROM instrument \
         WHERE status = 'ACTIVE' AND ($1::text IS NULL OR symbol ILIKE $1 OR name ILIKE $1) \
         ORDER BY symbol \
         LIMIT $2",
    )
    .bind(pattern)
    .bind(limit)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(records))
}

// ── Symbology (FIGI resolution) ───────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ResolveRequest {
    pub ticker: Option<String>,
    pub isin: Option<String>,
    pub cusip: Option<String>,
    pub figi: Option<String>,
    pub mic: Option<String>,
    pub exch_code: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BackfillRequest {
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BackfillResult {
    pub scanned: i64,
    pub stamped: i64,
    pub ambiguous: i64,
    pub unresolved: i64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ExpirySweepResult {
    /// Contracts whose UTC `expires_at` was derived or corrected from the venue
    /// calendar. Non-zero on the first run after seeding a calendar, or after
    /// correcting one; zero on a steady-state run.
    pub recomputed: u64,
    /// Instruments moved from ACTIVE to EXPIRED.
    pub expired: u64,
}

fn map_resolve_error(err: ResolveError) -> AdminError {
    match err {
        ResolveError::Db(e) => AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("database error: {e}"),
        },
        ResolveError::Engine(e) => AdminError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("symbology engine error: {e}"),
        },
    }
}

#[utoipa::path(
    post, path = "/admin/symbology/resolve", tag = "admin",
    request_body = ResolveRequest,
    responses((status = 200, description = "Resolution", body = ResolveOutcome)),
    security(("bearer_token" = []))
)]
pub async fn resolve_symbology(
    State(state): State<AppState>,
    Json(req): Json<ResolveRequest>,
) -> Result<Json<ResolveOutcome>, AdminError> {
    let query = InstrumentQuery {
        figi: req.figi,
        isin: req.isin,
        cusip: req.cusip,
        ticker: req.ticker,
        mic: req.mic,
        exch_code: req.exch_code,
        currency: None,
        market_sec_des: None,
    };
    let outcome = symbology_resolver::resolve(state.pool(), state.symbology().as_ref(), &query)
        .await
        .map_err(map_resolve_error)?;
    Ok(Json(outcome))
}

#[utoipa::path(
    post, path = "/admin/symbology/backfill", tag = "admin",
    request_body = BackfillRequest,
    responses((status = 200, description = "Backfill summary", body = BackfillResult)),
    security(("bearer_token" = []))
)]
pub async fn backfill_symbology(
    State(state): State<AppState>,
    Json(req): Json<BackfillRequest>,
) -> Result<Json<BackfillResult>, AdminError> {
    let limit = req.limit.unwrap_or(50).clamp(1, 500);
    info!(limit, "admin symbology backfill");
    let rows = sqlx::query(
        "SELECT id, symbol, venue FROM instrument \
         WHERE figi IS NULL AND status = 'ACTIVE' ORDER BY id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    let mut result = BackfillResult { scanned: 0, stamped: 0, ambiguous: 0, unresolved: 0 };
    for row in rows {
        result.scanned += 1;
        let symbol: String = row.get("symbol");
        let venue: String = row.get("venue");
        // Translate our MIC -> OpenFIGI exchCode for the lookup; mic pins the master match.
        let query = InstrumentQuery {
            ticker: Some(symbol),
            exch_code: symbology::openfigi_exch_code(&venue).map(str::to_string),
            mic: Some(venue),
            ..Default::default()
        };
        match symbology_resolver::resolve(state.pool(), state.symbology().as_ref(), &query).await {
            Ok(ResolveOutcome::Resolved { instrument_id: Some(_), .. }) => result.stamped += 1,
            Ok(ResolveOutcome::Ambiguous { .. }) => result.ambiguous += 1,
            _ => result.unresolved += 1,
        }
    }
    Ok(Json(result))
}

/// Run the instrument expiry pass now, instead of waiting for the hourly tick.
///
/// Same function the background job calls, so an on-demand run and a scheduled one
/// cannot diverge. Useful right after seeding a calendar (every dated contract gets
/// its instant immediately) and for confirming a correction took effect.
#[utoipa::path(
    post, path = "/admin/instruments/expiry-sweep", tag = "admin",
    responses((status = 200, description = "Expiry sweep summary", body = ExpirySweepResult)),
    security(("bearer_token" = []))
)]
pub async fn expiry_sweep(
    State(state): State<AppState>,
) -> Result<Json<ExpirySweepResult>, AdminError> {
    info!("admin expiry sweep");
    let (recomputed, expired) = crate::expiry::run_once(state.pool()).await.map_err(map_db_error)?;
    Ok(Json(ExpirySweepResult { recomputed, expired }))
}

// ── Setup status ───────────────────────────────────────────────────────────

/// One connection's classification, for the checklist below. Deliberately
/// the same three strings `RedactedCredentials` uses
/// (`configured`/`unconfigured`/`error`) rather than a boolean — "stored but
/// the master key can't open it" is a different operator problem from
/// "never configured", and collapsing them here would hide exactly the
/// misconfiguration this checklist exists to surface.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SetupConnectionStatus {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SetupCatalogStatus {
    pub instruments: i64,
    pub state: String,
}

/// The checklist an operator (or the cockpit's own onboarding banner) reads to
/// answer "what is left to configure" — nothing else in this system can
/// answer that question in one call today.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SetupStatus {
    pub database: String,
    /// `"default"` iff the admin console is still guarded by the built-in
    /// dev password rather than an operator-chosen one — see
    /// `admin_password_state`.
    pub admin_password: String,
    /// Whether a master key is configured. Never the key itself, nor even
    /// whether it is *valid* — an invalid key surfaces the same way every
    /// other credential-reading endpoint surfaces it, as a 500 from
    /// `resolve_master_key`, not folded into this field.
    pub master_key: String,
    pub brokers: Vec<SetupConnectionStatus>,
    pub feeds: Vec<SetupConnectionStatus>,
    pub catalog: SetupCatalogStatus,
    pub portfolios: i64,
}

/// Maps a decoded connection's credential state onto the checklist's three
/// strings. Pulled out of `setup_status` so the classification — not the
/// database round-trip — is what a unit test exercises.
fn connection_state_label<T>(state: &CredentialState<T>) -> &'static str {
    match state {
        CredentialState::Configured(_) => "configured",
        CredentialState::Unconfigured => "unconfigured",
        CredentialState::Error(_) => "error",
    }
}

/// `"empty"` below any instrument count is meaningful — an operator has not
/// yet synced a catalog at all — `"ok"` otherwise. Not a judgment about
/// *how much* catalog is enough; that is out of scope for a boolean-ish
/// checklist entry.
fn catalog_state(instruments: i64) -> &'static str {
    if instruments == 0 {
        "empty"
    } else {
        "ok"
    }
}

/// A fresh install and a properly operated one differ in exactly one way an
/// operator can forget to fix: the admin console password. Reported by
/// comparison against the same constant `main.rs` falls back to, so this can
/// never drift from what boot actually accepted.
fn admin_password_state(password: &str) -> &'static str {
    if password == crate::DEFAULT_ADMIN_PASSWORD {
        "default"
    } else {
        "set"
    }
}

/// What is left to configure. Reuses `credentials::load_brokers`/`load_feeds`
/// — the exact decode path `reload_connections` uses — so this checklist
/// cannot report a connection "configured" that the reload path would
/// actually refuse to register, or the reverse.
#[utoipa::path(
    get, path = "/admin/setup-status", tag = "admin",
    responses((status = 200, description = "OK", body = SetupStatus)),
    security(("bearer_token" = []))
)]
pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, AdminError> {
    let key = resolve_master_key()?;

    let brokers = crate::credentials::load_brokers(state.pool(), key.as_ref())
        .await
        .map_err(map_db_error)?
        .into_iter()
        .map(|c| SetupConnectionStatus { state: connection_state_label(&c.credentials).to_string(), code: c.code })
        .collect();

    let feeds = crate::credentials::load_feeds(state.pool(), key.as_ref())
        .await
        .map_err(map_db_error)?
        .into_iter()
        .map(|c| SetupConnectionStatus { state: connection_state_label(&c.credentials).to_string(), code: c.code })
        .collect();

    let instruments: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument")
        .fetch_one(state.pool())
        .await
        .map_err(map_db_error)?;

    let portfolios: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio")
        .fetch_one(state.pool())
        .await
        .map_err(map_db_error)?;

    Ok(Json(SetupStatus {
        database: "ok".to_string(),
        admin_password: admin_password_state(&state.admin_token).to_string(),
        master_key: if key.is_some() { "configured" } else { "unconfigured" }.to_string(),
        brokers,
        feeds,
        catalog: SetupCatalogStatus { state: catalog_state(instruments).to_string(), instruments },
        portfolios,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AdminError {
    status: StatusCode,
    message: String,
}

impl AdminError {
    fn not_found(resource: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("{resource} not found"),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

fn map_db_error(err: sqlx::Error) -> AdminError {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.code().as_deref() == Some("23505") {
            return AdminError {
                status: StatusCode::CONFLICT,
                message: "resource already exists".to_string(),
            };
        }
    }

    AdminError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("database error: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(code: &str, state: CredentialState<BrokerCredentials>) -> Connection<BrokerCredentials> {
        Connection {
            code: code.into(),
            kind: "ALPACA".into(),
            environment: Some("PAPER".into()),
            status: "ACTIVE".into(),
            credentials: state,
            credentials_updated_at: None,
        }
    }

    fn feed(code: &str, state: CredentialState<FeedCredentials>) -> Connection<FeedCredentials> {
        Connection {
            code: code.into(),
            kind: "DATABENTO".into(),
            environment: None,
            status: "ACTIVE".into(),
            credentials: state,
            credentials_updated_at: None,
        }
    }

    fn good_alpaca() -> CredentialState<BrokerCredentials> {
        CredentialState::Configured(BrokerCredentials::Alpaca { key: "k".into(), secret: "s".into() })
    }

    /// The gate must never fire on a normal running system or a fresh
    /// install: nothing stored, or everything stored decrypts fine.
    #[test]
    fn does_not_refuse_when_nothing_or_everything_decrypted() {
        assert!(!must_refuse_reload(&[], &[]), "nothing stored at all");

        let brokers = vec![broker("alpaca-paper", good_alpaca())];
        assert!(!must_refuse_reload(&brokers, &[]), "everything decrypts");
    }

    /// One bad row must never disarm every other one — the same
    /// partial-failure tolerance boot itself has via `nothing_decrypted`.
    #[test]
    fn does_not_refuse_on_a_partial_failure() {
        let brokers = vec![
            broker("alpaca-paper", good_alpaca()),
            broker("binance-paper", CredentialState::Error("bad key".into())),
        ];
        assert!(!must_refuse_reload(&brokers, &[]), "one bad row among good ones must not refuse");
    }

    /// The headline case this gate exists for: every broker credential fails
    /// to decrypt (e.g. a `rotate-key` run this process's in-memory master
    /// key no longer matches) and nothing else is stored to save it.
    #[test]
    fn refuses_when_every_broker_credential_fails_and_nothing_else_decrypted() {
        let brokers = vec![broker("alpaca-paper", CredentialState::Error("bad key".into()))];
        assert!(must_refuse_reload(&brokers, &[]));
    }

    /// The gate looks across *both* stores, not just brokers: an all-feed
    /// failure with no broker credentials stored at all must refuse too.
    #[test]
    fn refuses_when_only_feeds_are_stored_and_all_fail() {
        let feeds = vec![feed("databento-opra", CredentialState::Error("bad key".into()))];
        assert!(must_refuse_reload(&[], &feeds));
    }

    /// A broker credential decrypting fine must save a reload even when every
    /// feed credential fails — the two stores are OR'd together into one "did
    /// anything at all decrypt" verdict, matching `nothing_decrypted`'s own
    /// contract, which does not distinguish which store a decoded row came
    /// from.
    #[test]
    fn a_good_broker_saves_the_reload_even_if_every_feed_fails() {
        let brokers = vec![broker("alpaca-paper", good_alpaca())];
        let feeds = vec![feed("databento-opra", CredentialState::Error("bad key".into()))];
        assert!(!must_refuse_reload(&brokers, &feeds));
    }

    /// A connection this pass just restarted must never also be stopped,
    /// regardless of whether an adapter happens to already be there.
    #[test]
    fn a_registered_connection_is_never_stopped() {
        assert!(!should_stop_alpaca_stream(&reload::ConnectionOutcome::Registered, true));
        assert!(!should_stop_alpaca_stream(&reload::ConnectionOutcome::Registered, false));
    }

    /// The regression this function exists to prevent: an `Error`ed
    /// connection whose adapter was carried forward (see `build_registry`'s
    /// `Error` arm) must keep its execution stream running, or orders route
    /// while fills silently stop arriving.
    #[test]
    fn a_failed_connection_with_a_surviving_adapter_is_not_stopped() {
        assert!(!should_stop_alpaca_stream(
            &reload::ConnectionOutcome::Failed("bad key".into()),
            true,
        ));
    }

    /// The ordinary cases this function exists for: `Disabled`, `Unconfigured`,
    /// and a `Failed` with nothing left running all mean stop.
    #[test]
    fn disabled_unconfigured_and_unrecovered_failed_are_stopped() {
        assert!(should_stop_alpaca_stream(&reload::ConnectionOutcome::Disabled, false));
        assert!(should_stop_alpaca_stream(&reload::ConnectionOutcome::Unconfigured, false));
        assert!(should_stop_alpaca_stream(&reload::ConnectionOutcome::Failed("bad key".into()), false));
    }

    /// `Disabled` and `Unconfigured` are the deliberate-operator-state cases:
    /// an operator turning a feed off (or never having set it up) must not
    /// leave it silently streaming quotes on its old credential.
    #[test]
    fn disabled_and_unconfigured_feeds_are_stopped() {
        assert!(should_stop_databento_feed(&reload::ConnectionOutcome::Disabled));
        assert!(should_stop_databento_feed(&reload::ConnectionOutcome::Unconfigured));
    }

    /// The rule this task established, mirrored from the broker side: a feed
    /// whose *stored* credential just failed to decrypt is left running,
    /// because a stopped feed prices nothing. Unlike Alpaca there is no
    /// "adapter survived" input to check — the running task is simply never
    /// touched, so `should_stop_databento_feed` alone decides this.
    #[test]
    fn a_failed_feed_credential_is_not_stopped() {
        assert!(!should_stop_databento_feed(&reload::ConnectionOutcome::Failed("bad key".into())));
    }

    /// A feed this pass just restarted must never also be stopped.
    #[test]
    fn a_registered_feed_is_never_stopped() {
        assert!(!should_stop_databento_feed(&reload::ConnectionOutcome::Registered));
    }

    // ── Credential write endpoints ──────────────────────────────────────

    /// A failed test must stop before the write. This is the gate the whole
    /// feature rests on: a credential that cannot authenticate must never
    /// replace one that can.
    #[test]
    fn a_failed_test_blocks_the_write() {
        assert!(!should_persist(&Err("401 unauthorized".into())));
        assert!(should_persist(&Ok(())));
    }

    /// `persist_gate` is the seam between `TestOutcome` and `should_persist`:
    /// `Passed` and `NotTestable` (FIX — "we didn't check", not "it
    /// failed") both permit the write; only `Failed` withholds it.
    #[test]
    fn passed_and_not_testable_permit_the_write_only_failed_blocks_it() {
        assert!(should_persist(&persist_gate(&TestOutcome::Passed)));
        assert!(should_persist(&persist_gate(&TestOutcome::NotTestable("checked at session logon"))));
        assert!(!should_persist(&persist_gate(&TestOutcome::Failed("401 unauthorized".into()))));
    }

    /// The regression this function exists to prevent: a `LIVE` connection's
    /// own environment must reach `AlpacaAdapter::new`, not a hardcoded
    /// `"PAPER"` literal that would 401 a perfectly valid live credential.
    /// (Asserting the adapter's own base URL would need either a network
    /// call or a getter on `AlpacaAdapter`, which is not a file this task
    /// may touch — this asserts the value that actually reaches the
    /// constructor instead.)
    #[test]
    fn test_environment_uses_the_connections_own_environment_not_a_hardcoded_default() {
        assert_eq!(test_environment(Some("LIVE")), "LIVE");
        assert_eq!(test_environment(Some("PAPER")), "PAPER");
        assert_eq!(test_environment(None), "PAPER");
    }

    /// A non-string field value (e.g. a JSON number where the wire format
    /// expects a string) must be a 400 that names the problem — not axum's
    /// own extractor 422, which this endpoint's docs reserve for "the
    /// credential did not authenticate", and not a message that echoes the
    /// submitted value back.
    #[test]
    fn a_non_string_field_value_is_a_400_that_does_not_echo_it() {
        let raw = serde_json::json!({"key": "AKROUNDTRIP", "secret": 12345});
        let err = parse_submission(raw).expect_err("a numeric field value must be rejected");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(!err.message.contains("12345"), "the submitted value must not be echoed: {}", err.message);
    }

    /// A save response must carry the redacted view, never the submission.
    /// Built through `redact_connection` — the same function the handler
    /// calls — from a `Configured` credential holding a distinctive secret,
    /// so this can actually fail: the brief's original version asserted
    /// `!body.contains("SUPERSECRET")` against a `RedactedCredentials`
    /// literal that never contained "SUPERSECRET" to begin with, which
    /// cannot fail for any implementation.
    #[test]
    fn the_save_response_carries_no_submitted_secret() {
        let conn = broker(
            "alpaca-paper",
            CredentialState::Configured(BrokerCredentials::Alpaca {
                key: "AKLIVE".into(),
                secret: "SUPERSECRETVALUE".into(),
            }),
        );
        let body = serde_json::to_string(&SaveResponse {
            redacted: redact_connection(conn),
            tested: true,
            message: None,
            reload: None,
        })
        .expect("serialize");
        assert!(!body.contains("SUPERSECRETVALUE"), "submitted secret leaked into the save response: {body}");
        assert!(body.contains("alpaca-paper"));
    }

    /// Same regression, feed side: `redact_connection`'s generic dispatch
    /// (`ToRedactedFields`) must redact a `FeedCredentials::Databento` just
    /// as thoroughly as it redacts a broker credential.
    #[test]
    fn the_feed_save_response_carries_no_submitted_secret() {
        let conn = feed(
            "databento-opra",
            CredentialState::Configured(FeedCredentials::Databento { api_key: "SUPERSECRETFEEDKEY".into() }),
        );
        let body = serde_json::to_string(&SaveResponse {
            redacted: redact_connection(conn),
            tested: true,
            message: None,
            reload: None,
        })
        .expect("serialize");
        assert!(!body.contains("SUPERSECRETFEEDKEY"), "submitted secret leaked into the save response: {body}");
        assert!(body.contains("databento-opra"));
    }

    /// `test_response` is what both the `test` endpoint and the save path's
    /// `tested` flag draw from: a real pass reports `tested`, `ok`, and no
    /// message.
    #[test]
    fn test_response_reports_a_clean_pass() {
        let r = test_response(&TestOutcome::Passed);
        assert!(r.tested);
        assert!(r.ok);
        assert!(r.message.is_none());
    }

    /// FIX must never imply a pass it did not earn: `tested` and `ok` are
    /// both `false`, with the reason carried in `message`.
    #[test]
    fn test_response_reports_not_testable_honestly() {
        let r = test_response(&TestOutcome::NotTestable("checked at session logon"));
        assert!(!r.tested);
        assert!(!r.ok);
        assert_eq!(r.message.as_deref(), Some("checked at session logon"));
    }

    /// A rejected credential is `tested` (an attempt was made) but not `ok`,
    /// with the broker's own message carried through unchanged.
    #[test]
    fn test_response_reports_a_failure_with_its_message() {
        let r = test_response(&TestOutcome::Failed("401 unauthorized".into()));
        assert!(r.tested);
        assert!(!r.ok);
        assert_eq!(r.message.as_deref(), Some("401 unauthorized"));
    }

    /// Exercises the real seal/save/load/redact path against Postgres — the
    /// SQL text `save_broker` and `load_brokers` run has no coverage at all
    /// without a database. Mirrors `credentials.rs`'s own round trip: the
    /// handler's decision logic (`parse_broker`, `test_broker`,
    /// `persist_gate`) is exercised directly rather than through HTTP, since
    /// this test never starts a server.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live, migrated Postgres reachable via the usual POSTGRES_*
    /// config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn put_equivalent_round_trip_saves_and_redacts_with_no_secret_value() {
        use crate::secrets::parse_master_key;
        use crate::setup::database::config;

        let cfg = config::resolve(config::PostgresOverrides::default());
        let pool = sqlx::PgPool::connect(&cfg.url()).await.expect("connect");

        let code = "test-admin-credential-put-roundtrip";

        // Clean slate, in case a previous panicked run left this behind.
        sqlx::query("DELETE FROM oms.broker_connection WHERE code = $1")
            .bind(code)
            .execute(&pool)
            .await
            .expect("cleanup before");

        // IBKR, not Alpaca: `test_broker` is `NotTestable` for FIX, so this
        // round trip never reaches the network — see `credentials_api::test_broker`.
        sqlx::query(
            "INSERT INTO oms.broker_connection (code, broker_code, environment, status) \
             VALUES ($1, 'IBKR', 'PAPER', 'ACTIVE')",
        )
        .bind(code)
        .execute(&pool)
        .await
        .expect("insert test broker_connection row");

        let key = parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("key");

        let submission = CredentialSubmission {
            fields: [
                ("host", "fix.roundtrip.test"),
                ("port", "4101"),
                ("sender_comp_id", "SENDERROUNDTRIP"),
                ("target_comp_id", "TARGETROUNDTRIP"),
                ("password", "ROUNDTRIPSECRETPW"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        };

        // decrypt existing (none yet) → parse merged → test → persist
        let parsed = credentials_api::parse_broker("IBKR", None, &submission).expect("parse_broker");
        let outcome = credentials_api::test_broker(&parsed, "PAPER").await;
        assert!(
            should_persist(&persist_gate(&outcome)),
            "IBKR is NotTestable and must still be allowed to persist"
        );
        crate::credentials::save_broker(&pool, &key, code, &parsed).await.expect("save_broker");

        let brokers = crate::credentials::load_brokers(&pool, Some(&key)).await.expect("load_brokers");
        let conn = brokers.into_iter().find(|c| c.code == code).expect("row present");
        let redacted = redact_connection(conn);

        assert_eq!(redacted.state, "configured");
        let body = serde_json::to_string(&redacted).expect("serialize");
        assert!(!body.contains("ROUNDTRIPSECRETPW"), "submitted secret leaked into the redacted view: {body}");

        // Leave the table as we found it.
        sqlx::query("DELETE FROM oms.broker_connection WHERE code = $1")
            .bind(code)
            .execute(&pool)
            .await
            .expect("cleanup after");
    }

    #[test]
    fn a_canonical_broker_code_is_accepted() {
        assert!(validate_broker_code("ALPACA").is_ok());
        assert!(validate_broker_code("BINANCE").is_ok());
        assert!(validate_broker_code("IBKR").is_ok());
    }

    /// The registry keys adapters are registered under are case-sensitive
    /// literals (`reload.rs`); a code that only differs in case looks fine at
    /// save time and at reload, then 503s every order — the exact hazard this
    /// validation exists to close.
    #[test]
    fn a_differently_cased_broker_code_is_rejected() {
        let err = validate_broker_code("binance").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn an_unknown_broker_code_is_rejected() {
        let err = validate_broker_code("COINBASE").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn the_error_names_the_accepted_values() {
        let err = validate_broker_code("nope").unwrap_err();
        assert!(err.message.contains("ALPACA"), "{}", err.message);
        assert!(err.message.contains("BINANCE"), "{}", err.message);
        assert!(err.message.contains("IBKR"), "{}", err.message);
    }

    #[test]
    fn catalog_state_reflects_the_count() {
        assert_eq!(catalog_state(0), "empty");
        assert_eq!(catalog_state(14_000), "ok");
    }

    /// The checklist exists to tell an operator what is left. A default admin
    /// password must be reported, because it is the one thing a fresh install
    /// has that a finished one must not.
    #[test]
    fn a_default_admin_password_is_reported() {
        assert_eq!(admin_password_state("openoms-dev"), "default");
        assert_eq!(admin_password_state("something-else"), "set");
    }

    // ── Session revocation ──────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn revoking_sessions_reports_how_many_it_killed() {
        let pool = test_pool().await;
        let (principal_id, _code) = seed_principal(&pool, "admin-revoke-sessions-test").await;
        crate::sessions::create_session(&pool, principal_id, &Default::default(), None)
            .await
            .expect("session");

        let revoked = crate::sessions::revoke_all_for_principal(&pool, principal_id)
            .await
            .expect("revoke");

        assert_eq!(revoked, 1);
    }

    // ── test plumbing ────────────────────────────────────────────────────

    /// `main` loads .env before resolving config; a test binary does not, so
    /// without this the test resolves a different database than the server
    /// runs against. The `oms` role carries `search_path = oms, public`
    /// (db/access/roles.sql); these are the admin credentials, so set it here.
    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// `principal.code` is `UNIQUE` and these tests leave their rows behind,
    /// so the code is suffixed with the row's own id. Returns the id and the
    /// code actually inserted, since callers assert against it.
    async fn seed_principal(pool: &sqlx::PgPool, code: &str) -> (Uuid, String) {
        let id = Uuid::new_v4();
        let code = format!("{code}-{id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')",
        )
        .bind(id)
        .bind(&code)
        .execute(pool)
        .await
        .expect("seed principal");
        (id, code)
    }
}
