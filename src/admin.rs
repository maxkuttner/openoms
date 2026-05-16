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
use crate::domain::identity::{Account, Book, BrokerInstrument, Grant, Instrument, Principal};

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
pub struct CreateBook {
    pub code: String,
    pub name: String,
    pub status: String,
    pub base_currency: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateBook {
    pub code: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub base_currency: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateAccount {
    pub code: String,
    pub broker_code: String,
    pub environment: String,
    pub external_account_ref: String,
    pub status: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateAccount {
    pub code: Option<String>,
    pub broker_code: Option<String>,
    pub environment: Option<String>,
    pub external_account_ref: Option<String>,
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
        INSERT INTO oms_principal (
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
            FROM oms_principal
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
            FROM oms_principal
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
        FROM oms_principal
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
        UPDATE oms_principal
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
    post, path = "/admin/books", tag = "admin",
    request_body = CreateBook,
    responses(
        (status = 200, description = "Created", body = Book),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_book(
    State(state): State<AppState>,
    Json(payload): Json<CreateBook>,
) -> Result<Json<Book>, AdminError> {
    info!(code = %payload.code, name = %payload.name, "admin create book");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Book>(
        r#"
        INSERT INTO oms_book (
            id,
            code,
            name,
            status,
            base_currency
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING id, code, name, status, base_currency, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.name)
    .bind(payload.status)
    .bind(payload.base_currency)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/books", tag = "admin",
    responses(
        (status = 200, description = "OK", body = [Book]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_books(
    State(state): State<AppState>,
) -> Result<Json<Vec<Book>>, AdminError> {
    info!("admin list books");
    let records = sqlx::query_as::<_, Book>(
        r#"
        SELECT id, code, name, status, base_currency, created_at, updated_at
        FROM oms_book
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/books/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Book ID")),
    responses(
        (status = 200, description = "OK", body = Book),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_book(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Book>, AdminError> {
    info!(book_id = %id, "admin get book");
    let record = sqlx::query_as::<_, Book>(
        r#"
        SELECT id, code, name, status, base_currency, created_at, updated_at
        FROM oms_book
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("book"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/books/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Book ID")),
    request_body = UpdateBook,
    responses(
        (status = 200, description = "Updated", body = Book),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_book(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateBook>,
) -> Result<Json<Book>, AdminError> {
    info!(book_id = %id, "admin update book");
    let record = sqlx::query_as::<_, Book>(
        r#"
        UPDATE oms_book
        SET
            code = COALESCE($1, code),
            name = COALESCE($2, name),
            status = COALESCE($3, status),
            base_currency = COALESCE($4, base_currency),
            updated_at = now()
        WHERE id = $5
        RETURNING id, code, name, status, base_currency, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.name)
    .bind(payload.status)
    .bind(payload.base_currency)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("book"))?;

    Ok(Json(record))
}

#[utoipa::path(
    post, path = "/admin/accounts", tag = "admin",
    request_body = CreateAccount,
    responses(
        (status = 200, description = "Created", body = Account),
        (status = 400, description = "Invalid environment (must be PAPER or LIVE)"),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccount>,
) -> Result<Json<Account>, AdminError> {
    if payload.environment != "PAPER" && payload.environment != "LIVE" {
        return Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: "environment must be PAPER or LIVE".to_string(),
        });
    }
    info!(code = %payload.code, broker_code = %payload.broker_code, environment = %payload.environment, "admin create account");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO oms_account (
            id,
            code,
            broker_code,
            environment,
            external_account_ref,
            status
        ) VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, code, broker_code, environment, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.broker_code)
    .bind(payload.environment)
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
        SELECT id, code, broker_code, environment, external_account_ref, status, created_at, updated_at
        FROM oms_account
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
        SELECT id, code, broker_code, environment, external_account_ref, status, created_at, updated_at
        FROM oms_account
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
        (status = 400, description = "Invalid environment"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateAccount>,
) -> Result<Json<Account>, AdminError> {
    if let Some(ref env) = payload.environment {
        if env != "PAPER" && env != "LIVE" {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: "environment must be PAPER or LIVE".to_string(),
            });
        }
    }
    info!(account_id = %id, "admin update account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        UPDATE oms_account
        SET
            code = COALESCE($1, code),
            broker_code = COALESCE($2, broker_code),
            environment = COALESCE($3, environment),
            external_account_ref = COALESCE($4, external_account_ref),
            status = COALESCE($5, status),
            updated_at = now()
        WHERE id = $6
        RETURNING id, code, broker_code, environment, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.broker_code)
    .bind(payload.environment)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("account"))?;

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
        FROM oms_api_key
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
        INSERT INTO oms_api_key (principal_id, key_id, secret_hash, name)
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
        UPDATE oms_api_key SET revoked_at = now()
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
    pub book_id: Uuid,
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
        (status = 409, description = "Grant already exists for this principal/book/account"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_grant(
    State(state): State<AppState>,
    Path(principal_id): Path<Uuid>,
    Json(payload): Json<CreateGrant>,
) -> Result<Json<Grant>, AdminError> {
    info!(principal_id = %principal_id, book_id = %payload.book_id, account_id = %payload.account_id, "admin create grant");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Grant>(
        r#"
        INSERT INTO oms_principal_book_account_grant (id, principal_id, book_id, account_id, can_trade, can_view, can_allocate)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, principal_id, book_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(principal_id)
    .bind(payload.book_id)
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
        SELECT id, principal_id, book_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
        FROM oms_principal_book_account_grant
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
        UPDATE oms_principal_book_account_grant
        SET
            can_trade    = COALESCE($1, can_trade),
            can_view     = COALESCE($2, can_view),
            can_allocate = COALESCE($3, can_allocate),
            updated_at   = now()
        WHERE id = $4 AND principal_id = $5
        RETURNING id, principal_id, book_id, account_id, can_trade, can_view, can_allocate, created_at, updated_at
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
        "DELETE FROM oms_principal_book_account_grant WHERE id = $1 AND principal_id = $2",
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

// ── Instrument management ─────────────────────────────────────────────────────

const VALID_ASSET_CLASSES: &[&str] = &["EQUITY", "OPTION", "FUTURE", "FOREX", "CRYPTO", "FIXED_INCOME"];
const VALID_INSTRUMENT_STATUSES: &[&str] = &["ACTIVE", "INACTIVE", "HALTED"];

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateInstrument {
    pub code: String,
    pub symbol: String,
    pub asset_class: String,
    pub name: String,
    pub currency: String,
    pub exchange: Option<String>,
    pub status: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateInstrument {
    pub code: Option<String>,
    pub symbol: Option<String>,
    pub asset_class: Option<String>,
    pub name: Option<String>,
    pub currency: Option<String>,
    pub exchange: Option<String>,
    pub status: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[utoipa::path(
    post, path = "/admin/instruments", tag = "admin",
    request_body = CreateInstrument,
    responses(
        (status = 200, description = "Created", body = Instrument),
        (status = 400, description = "Invalid asset_class or status"),
        (status = 409, description = "Already exists"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_instrument(
    State(state): State<AppState>,
    Json(payload): Json<CreateInstrument>,
) -> Result<Json<Instrument>, AdminError> {
    if !VALID_ASSET_CLASSES.contains(&payload.asset_class.as_str()) {
        return Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: format!("asset_class must be one of: {}", VALID_ASSET_CLASSES.join(", ")),
        });
    }
    let status = payload.status.as_deref().unwrap_or("ACTIVE");
    if !VALID_INSTRUMENT_STATUSES.contains(&status) {
        return Err(AdminError {
            status: StatusCode::BAD_REQUEST,
            message: format!("status must be one of: {}", VALID_INSTRUMENT_STATUSES.join(", ")),
        });
    }
    info!(code = %payload.code, symbol = %payload.symbol, "admin create instrument");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Instrument>(
        r#"
        INSERT INTO oms_instrument (id, code, symbol, asset_class, name, currency, exchange, status, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, code, symbol, asset_class, name, currency, exchange, status, metadata, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.symbol)
    .bind(payload.asset_class)
    .bind(payload.name)
    .bind(payload.currency)
    .bind(payload.exchange)
    .bind(status)
    .bind(payload.metadata)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/instruments", tag = "admin",
    responses(
        (status = 200, description = "OK", body = [Instrument]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_instruments(
    State(state): State<AppState>,
) -> Result<Json<Vec<Instrument>>, AdminError> {
    info!("admin list instruments");
    let records = sqlx::query_as::<_, Instrument>(
        r#"
        SELECT id, code, symbol, asset_class, name, currency, exchange, status, metadata, created_at, updated_at
        FROM oms_instrument
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/instruments/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Instrument ID")),
    responses(
        (status = 200, description = "OK", body = Instrument),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_instrument(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Instrument>, AdminError> {
    info!(instrument_id = %id, "admin get instrument");
    let record = sqlx::query_as::<_, Instrument>(
        r#"
        SELECT id, code, symbol, asset_class, name, currency, exchange, status, metadata, created_at, updated_at
        FROM oms_instrument
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("instrument"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/instruments/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Instrument ID")),
    request_body = UpdateInstrument,
    responses(
        (status = 200, description = "Updated", body = Instrument),
        (status = 400, description = "Invalid asset_class or status"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_instrument(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateInstrument>,
) -> Result<Json<Instrument>, AdminError> {
    if let Some(ref ac) = payload.asset_class {
        if !VALID_ASSET_CLASSES.contains(&ac.as_str()) {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: format!("asset_class must be one of: {}", VALID_ASSET_CLASSES.join(", ")),
            });
        }
    }
    if let Some(ref s) = payload.status {
        if !VALID_INSTRUMENT_STATUSES.contains(&s.as_str()) {
            return Err(AdminError {
                status: StatusCode::BAD_REQUEST,
                message: format!("status must be one of: {}", VALID_INSTRUMENT_STATUSES.join(", ")),
            });
        }
    }
    info!(instrument_id = %id, "admin update instrument");
    let record = sqlx::query_as::<_, Instrument>(
        r#"
        UPDATE oms_instrument
        SET
            code        = COALESCE($1, code),
            symbol      = COALESCE($2, symbol),
            asset_class = COALESCE($3, asset_class),
            name        = COALESCE($4, name),
            currency    = COALESCE($5, currency),
            exchange    = COALESCE($6, exchange),
            status      = COALESCE($7, status),
            metadata    = COALESCE($8, metadata),
            updated_at  = now()
        WHERE id = $9
        RETURNING id, code, symbol, asset_class, name, currency, exchange, status, metadata, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.symbol)
    .bind(payload.asset_class)
    .bind(payload.name)
    .bind(payload.currency)
    .bind(payload.exchange)
    .bind(payload.status)
    .bind(payload.metadata)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("instrument"))?;

    Ok(Json(record))
}

// ── Broker instrument mapping ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateBrokerInstrument {
    pub instrument_id: Uuid,
    pub broker_code: String,
    pub broker_symbol: String,
    pub broker_exchange: Option<String>,
    pub is_tradeable: Option<bool>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateBrokerInstrument {
    pub broker_symbol: Option<String>,
    pub broker_exchange: Option<String>,
    pub is_tradeable: Option<bool>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct BrokerInstrumentFilter {
    pub instrument_id: Option<Uuid>,
    pub broker_code: Option<String>,
}

#[utoipa::path(
    post, path = "/admin/broker-instruments", tag = "admin",
    request_body = CreateBrokerInstrument,
    responses(
        (status = 200, description = "Created", body = BrokerInstrument),
        (status = 409, description = "Mapping already exists for this instrument/broker pair"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_broker_instrument(
    State(state): State<AppState>,
    Json(payload): Json<CreateBrokerInstrument>,
) -> Result<Json<BrokerInstrument>, AdminError> {
    info!(instrument_id = %payload.instrument_id, broker_code = %payload.broker_code, "admin create broker instrument");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, BrokerInstrument>(
        r#"
        INSERT INTO oms_broker_instrument (id, instrument_id, broker_code, broker_symbol, broker_exchange, is_tradeable, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, instrument_id, broker_code, broker_symbol, broker_exchange, is_tradeable, metadata, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.instrument_id)
    .bind(payload.broker_code)
    .bind(payload.broker_symbol)
    .bind(payload.broker_exchange)
    .bind(payload.is_tradeable.unwrap_or(true))
    .bind(payload.metadata)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

#[utoipa::path(
    get, path = "/admin/broker-instruments", tag = "admin",
    params(BrokerInstrumentFilter),
    responses(
        (status = 200, description = "OK", body = [BrokerInstrument]),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_broker_instruments(
    State(state): State<AppState>,
    Query(filter): Query<BrokerInstrumentFilter>,
) -> Result<Json<Vec<BrokerInstrument>>, AdminError> {
    info!("admin list broker instruments");
    let records = sqlx::query_as::<_, BrokerInstrument>(
        r#"
        SELECT id, instrument_id, broker_code, broker_symbol, broker_exchange, is_tradeable, metadata, created_at, updated_at
        FROM oms_broker_instrument
        WHERE ($1::uuid IS NULL OR instrument_id = $1)
          AND ($2::text IS NULL OR broker_code = $2)
        ORDER BY created_at DESC
        "#,
    )
    .bind(filter.instrument_id)
    .bind(filter.broker_code)
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

#[utoipa::path(
    get, path = "/admin/broker-instruments/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Broker instrument mapping ID")),
    responses(
        (status = 200, description = "OK", body = BrokerInstrument),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_broker_instrument(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<BrokerInstrument>, AdminError> {
    info!(broker_instrument_id = %id, "admin get broker instrument");
    let record = sqlx::query_as::<_, BrokerInstrument>(
        r#"
        SELECT id, instrument_id, broker_code, broker_symbol, broker_exchange, is_tradeable, metadata, created_at, updated_at
        FROM oms_broker_instrument
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("broker instrument"))?;

    Ok(Json(record))
}

#[utoipa::path(
    patch, path = "/admin/broker-instruments/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Broker instrument mapping ID")),
    request_body = UpdateBrokerInstrument,
    responses(
        (status = 200, description = "Updated", body = BrokerInstrument),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_broker_instrument(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateBrokerInstrument>,
) -> Result<Json<BrokerInstrument>, AdminError> {
    info!(broker_instrument_id = %id, "admin update broker instrument");
    let record = sqlx::query_as::<_, BrokerInstrument>(
        r#"
        UPDATE oms_broker_instrument
        SET
            broker_symbol   = COALESCE($1, broker_symbol),
            broker_exchange = COALESCE($2, broker_exchange),
            is_tradeable    = COALESCE($3, is_tradeable),
            metadata        = COALESCE($4, metadata),
            updated_at      = now()
        WHERE id = $5
        RETURNING id, instrument_id, broker_code, broker_symbol, broker_exchange, is_tradeable, metadata, created_at, updated_at
        "#,
    )
    .bind(payload.broker_symbol)
    .bind(payload.broker_exchange)
    .bind(payload.is_tradeable)
    .bind(payload.metadata)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("broker instrument"))?;

    Ok(Json(record))
}

#[utoipa::path(
    delete, path = "/admin/broker-instruments/{id}", tag = "admin",
    params(("id" = Uuid, Path, description = "Broker instrument mapping ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_broker_instrument(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AdminError> {
    info!(broker_instrument_id = %id, "admin delete broker instrument");
    let result = sqlx::query("DELETE FROM oms_broker_instrument WHERE id = $1")
        .bind(id)
        .execute(state.pool())
        .await
        .map_err(map_db_error)?;

    if result.rows_affected() == 0 {
        return Err(AdminError::not_found("broker instrument"));
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
