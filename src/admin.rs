use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::domain::identity::{Account, Book, Principal};

#[derive(Debug, Deserialize)]
pub struct CreatePrincipal {
    pub code: String,
    pub principal_type: String,
    pub external_subject: Option<String>,
    pub display_name: Option<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePrincipal {
    pub code: Option<String>,
    pub principal_type: Option<String>,
    pub external_subject: Option<String>,
    pub display_name: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateBook {
    pub code: String,
    pub name: String,
    pub status: String,
    pub base_currency: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBook {
    pub code: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub base_currency: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateAccount {
    pub code: String,
    pub broker_code: String,
    pub external_account_ref: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAccount {
    pub code: Option<String>,
    pub broker_code: Option<String>,
    pub external_account_ref: Option<String>,
    pub status: Option<String>,
}

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

pub async fn list_principals(
    State(state): State<AppState>,
) -> Result<Json<Vec<Principal>>, AdminError> {
    info!("admin list principals");
    let records = sqlx::query_as::<_, Principal>(
        r#"
        SELECT id, code, principal_type, external_subject, display_name, status, created_at, updated_at
        FROM oms_principal
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

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

pub async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(code = %payload.code, broker_code = %payload.broker_code, "admin create account");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO oms_account (
            id,
            code,
            broker_code,
            external_account_ref,
            status
        ) VALUES ($1, $2, $3, $4, $5)
        RETURNING id, code, broker_code, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.broker_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}

pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<Vec<Account>>, AdminError> {
    info!("admin list accounts");
    let records = sqlx::query_as::<_, Account>(
        r#"
        SELECT id, code, broker_code, external_account_ref, status, created_at, updated_at
        FROM oms_account
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}

pub async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Account>, AdminError> {
    info!(account_id = %id, "admin get account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        SELECT id, code, broker_code, external_account_ref, status, created_at, updated_at
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

pub async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(account_id = %id, "admin update account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        UPDATE oms_account
        SET
            code = COALESCE($1, code),
            broker_code = COALESCE($2, broker_code),
            external_account_ref = COALESCE($3, external_account_ref),
            status = COALESCE($4, status),
            updated_at = now()
        WHERE id = $5
        RETURNING id, code, broker_code, external_account_ref, status, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.broker_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .bind(id)
    .fetch_optional(state.pool())
    .await
    .map_err(map_db_error)?
    .ok_or_else(|| AdminError::not_found("account"))?;

    Ok(Json(record))
}

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
