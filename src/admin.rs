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
use crate::domain::identity::{Account, BrokerConnection, Portfolio, Grant, Principal};
use crate::recon::{run_reconciliation, ReconError, ReconSummary};
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
    let key_id = format!("ak_{}", Uuid::new_v4().simple());

    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    raw[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let plaintext_secret = format!("sk_{}", general_purpose::URL_SAFE_NO_PAD.encode(raw));

    info!(principal_id = %principal_id, key_id = %key_id, "admin register key");

    let secret_to_hash = plaintext_secret.clone();
    let secret_hash = tokio::task::spawn_blocking(move || bcrypt::hash(&secret_to_hash, 12))
        .await
        .map_err(|_| AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "hash task failed".to_string(),
        })?
        .map_err(|_| AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "failed to hash secret".to_string(),
        })?;

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

// ── Custodian reconciliation ──────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RunReconRequest {
    pub broker_connection_code: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ReconRunRow {
    pub id: Uuid,
    pub broker_connection_code: String,
    pub oms_count: i32,
    pub custodian_count: i32,
    pub break_count: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ReconBreakRow {
    pub id: Uuid,
    pub recon_run_id: Uuid,
    pub instrument_id: Option<String>,
    pub symbol: Option<String>,
    pub oms_qty: f64,
    pub custodian_qty: f64,
    pub diff: f64,
    pub kind: String,
    pub created_at: DateTime<Utc>,
}

fn map_recon_error(err: ReconError) -> AdminError {
    match err {
        ReconError::NotFound(m) => AdminError { status: StatusCode::NOT_FOUND, message: m },
        ReconError::Unsupported(b) => AdminError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: format!("reconciliation not supported for broker {b}"),
        },
        ReconError::NoAdapter(m) => AdminError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("no adapter configured for {m}"),
        },
        ReconError::Broker(m) => AdminError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("custodian error: {m}"),
        },
        ReconError::Db(e) => AdminError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("database error: {e}"),
        },
    }
}

#[utoipa::path(
    post, path = "/admin/recon/run", tag = "admin",
    request_body = RunReconRequest,
    responses(
        (status = 200, description = "Reconciliation run", body = ReconSummary),
        (status = 404, description = "Broker connection not found"),
        (status = 422, description = "Broker not supported for reconciliation"),
        (status = 502, description = "Custodian fetch failed"),
    ),
    security(("bearer_token" = []))
)]
pub async fn run_recon(
    State(state): State<AppState>,
    Json(req): Json<RunReconRequest>,
) -> Result<Json<ReconSummary>, AdminError> {
    info!(broker_connection_code = %req.broker_connection_code, "admin run reconciliation");
    let summary = run_reconciliation(
        state.pool(),
        state.registry().as_ref(),
        state.symbology().as_ref(),
        &req.broker_connection_code,
    )
    .await
    .map_err(map_recon_error)?;
    Ok(Json(summary))
}

#[utoipa::path(
    get, path = "/admin/recon/runs", tag = "admin",
    responses((status = 200, description = "OK", body = [ReconRunRow])),
    security(("bearer_token" = []))
)]
pub async fn list_recon_runs(State(state): State<AppState>) -> Result<Json<Vec<ReconRunRow>>, AdminError> {
    let rows = sqlx::query_as::<_, ReconRunRow>(
        "SELECT id, broker_connection_code, oms_count, custodian_count, break_count, started_at, finished_at \
         FROM recon_run ORDER BY started_at DESC LIMIT 100",
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(rows))
}

#[utoipa::path(
    get, path = "/admin/recon/runs/{id}/breaks", tag = "admin",
    params(("id" = Uuid, Path, description = "Recon run ID")),
    responses((status = 200, description = "OK", body = [ReconBreakRow])),
    security(("bearer_token" = []))
)]
pub async fn list_recon_breaks(
    State(state): State<AppState>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Vec<ReconBreakRow>>, AdminError> {
    let rows = sqlx::query_as::<_, ReconBreakRow>(
        "SELECT id, recon_run_id, instrument_id, symbol, \
                oms_qty::float8 AS oms_qty, custodian_qty::float8 AS custodian_qty, \
                diff::float8 AS diff, kind, created_at \
         FROM recon_break WHERE recon_run_id = $1 ORDER BY created_at",
    )
    .bind(run_id)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;
    Ok(Json(rows))
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
    pub source_type: Option<String>,
    pub source_code: Option<String>,
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
    let source_type = req.source_type.unwrap_or_else(|| "MANUAL".to_string());
    let source_code = req.source_code.unwrap_or_else(|| "MANUAL".to_string());
    let outcome = symbology_resolver::resolve(
        state.pool(),
        state.symbology().as_ref(),
        &query,
        &source_type,
        &source_code,
    )
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
        // exch_code "US" drives the OpenFIGI lookup (US-equity assumption for the current
        // universe); mic = our venue is used to pin the master match.
        let query = InstrumentQuery {
            ticker: Some(symbol),
            exch_code: Some("US".to_string()),
            mic: Some(venue),
            ..Default::default()
        };
        match symbology_resolver::resolve(state.pool(), state.symbology().as_ref(), &query, "OPENFIGI", "OPENFIGI").await {
            Ok(ResolveOutcome::Resolved { instrument_id: Some(_), .. }) => result.stamped += 1,
            Ok(ResolveOutcome::Ambiguous { .. }) => result.ambiguous += 1,
            _ => result.unresolved += 1,
        }
    }
    Ok(Json(result))
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
