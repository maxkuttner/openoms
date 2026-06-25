use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use base64::engine::{general_purpose, Engine};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::domain::identity::{Account, BrokerConnection, Portfolio, Grant, Principal};

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
    pub account_id: Uuid,
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
        (status = 409, description = "Grant already exists for this principal/portfolio/account"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_grant(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
    Json(payload): Json<CreateGrant>,
) -> Result<Json<Grant>, AdminError> {
    info!(principal_id = %principal_id, portfolio_id = %payload.portfolio_id, account_id = %payload.account_id, "admin create grant");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Grant>(
        r#"
        INSERT INTO principal_portfolio_account_grant (id, principal_id, portfolio_id, account_id, can_trade, can_view, can_allocate)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, principal_id, portfolio_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(principal_id)
    .bind(payload.portfolio_id)
    .bind(payload.account_id)
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
        SELECT id, principal_id, portfolio_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
        FROM principal_portfolio_account_grant
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
        UPDATE principal_portfolio_account_grant
        SET
            can_trade    = COALESCE($1, can_trade),
            can_view     = COALESCE($2, can_view),
            can_allocate = COALESCE($3, can_allocate),
            updated_at   = now()
        WHERE id = $4 AND principal_id = $5
        RETURNING id, principal_id, portfolio_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
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
        "DELETE FROM principal_portfolio_account_grant WHERE id = $1 AND principal_id = $2",
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
