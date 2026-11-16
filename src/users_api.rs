//! Admin API for users: add, approve or reject sign-ups, role, portfolio
//! access, disable and API keys.
//! Admin-token routes (`/admin/users*`), HUMAN principals only; strategies
//! keep their own admin surface. Every mutation writes an `audit_event`.

use axum::extract::{Path, State};
use axum::Extension;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::admin::{ApiKeyRecord, CreateKey};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::ApiError;

/// A user's effective access on one portfolio: the highest across their groups.
#[derive(Debug, Serialize)]
pub struct UserPortfolio {
    pub portfolio_id: Uuid,
    pub code: String,
    pub access: &'static str,
}

#[derive(Debug, Serialize)]
pub struct UserGroupRef {
    pub id: Uuid,
    pub name: String,
}


#[derive(Debug, Serialize)]
pub struct UserRow {
    pub principal_id: Uuid,
    pub code: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub groups: Vec<UserGroupRef>,
    pub portfolios: Vec<UserPortfolio>,
    /// Host of the issuer the user last signed in with.
    pub sign_in: Option<String>,
    pub status: String,
    /// Holds the admin capability (opens the cockpit).
    pub admin: bool,
    /// The account role: `admin`, `user` or `pending` (see `accounts`).
    pub account_role: &'static str,
}

/// `POST /admin/users`: a local account with a password the admin passes on.
/// No `Debug`: it carries a password.
#[derive(Deserialize)]
pub struct AddUser {
    pub name: String,
    pub email: String,
    pub password: String,
    pub role: crate::accounts::AccountRole,
}

#[derive(Debug, Serialize)]
pub struct AddedUser {
    pub user_id: Uuid,
}

/// `POST /admin/users/:id/approve`: activate a pending account as a `user`,
/// in these groups.
#[derive(Debug, Default, Deserialize)]
pub struct Approve {
    #[serde(default)]
    pub groups: Vec<Uuid>,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn db(err: sqlx::Error) -> ApiError {
    crate::handlers::internal("database error", err)
}

fn is_violation(err: &sqlx::Error, check: impl Fn(&dyn sqlx::error::DatabaseError) -> bool) -> bool {
    matches!(err, sqlx::Error::Database(d) if check(d.as_ref()))
}

/// `https://login.example.org/realms/x` -> `login.example.org`.
fn issuer_host(issuer: &str) -> String {
    let rest = issuer.split_once("://").map_or(issuer, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: &AuthContext,
    action: &str,
    target: Uuid,
    detail: serde_json::Value,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO audit_event (actor, action, target_principal_id, detail) VALUES ($1, $2, $3, $4)")
        .bind(&actor.principal_code)
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

/// Lock a HUMAN principal and return its status; strategies and unknown ids are 404.
async fn lock_human(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar("SELECT status FROM principal WHERE id = $1 AND principal_type = 'HUMAN' FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "user not found"))
}

/// Users (HUMAN only), newest first; `only` narrows to one.
async fn fetch_users(pool: &PgPool, only: Option<Uuid>) -> Result<Vec<UserRow>, ApiError> {
    let rows = sqlx::query(
        "SELECT p.id, p.code, p.display_name, p.email, p.status, \
                (SELECT i.issuer FROM principal_identity i \
                  WHERE i.principal_id = p.id AND i.last_login_at IS NOT NULL \
                  ORDER BY i.last_login_at DESC LIMIT 1) AS issuer, \
                EXISTS (SELECT 1 FROM principal_admin a WHERE a.principal_id = p.id) AS admin \
           FROM principal p \
          WHERE p.principal_type = 'HUMAN' AND ($1::uuid IS NULL OR p.id = $1) \
          ORDER BY p.created_at DESC, p.code",
    )
    .bind(only)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let grants = sqlx::query(
        "SELECT g.principal_id, g.portfolio_id, pf.code, g.role_code \
           FROM portfolio_access g JOIN portfolio pf ON pf.id = g.portfolio_id \
          WHERE g.can_view AND ($1::uuid IS NULL OR g.principal_id = $1) \
          ORDER BY pf.code",
    )
    .bind(only)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let groups = sqlx::query(
        "SELECT m.principal_id, g.id, g.name FROM user_group_member m JOIN user_group g ON g.id = m.group_id \
          WHERE ($1::uuid IS NULL OR m.principal_id = $1) ORDER BY lower(g.name)",
    )
    .bind(only)
    .fetch_all(pool)
    .await
    .map_err(db)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            UserRow {
                principal_id: id,
                code: r.get("code"),
                display_name: r.get("display_name"),
                email: r.get("email"),
                groups: groups
                    .iter()
                    .filter(|g| g.get::<Uuid, _>("principal_id") == id)
                    .map(|g| UserGroupRef { id: g.get("id"), name: g.get("name") })
                    .collect(),
                portfolios: grants
                    .iter()
                    .filter(|g| g.get::<Uuid, _>("principal_id") == id)
                    .map(|g| UserPortfolio {
                        portfolio_id: g.get("portfolio_id"),
                        code: g.get("code"),
                        access: match g.get::<Option<String>, _>("role_code").as_deref() {
                            Some("pm") => "manage",
                            Some("trader") => "trade",
                            _ => "view",
                        },
                    })
                    .collect(),
                sign_in: r.get::<Option<String>, _>("issuer").map(|i| issuer_host(&i)),
                status: r.get("status"),
                account_role: match (r.get::<String, _>("status").as_str(), r.get::<bool, _>("admin")) {
                    ("PENDING", _) => "pending",
                    (_, true) => "admin",
                    _ => "user",
                },
                admin: r.get("admin"),
            }
        })
        .collect())
}

pub async fn list_users(State(state): State<AppState>) -> Result<Json<Vec<UserRow>>, ApiError> {
    Ok(Json(fetch_users(state.pool(), None).await?))
}

/// Add a local account directly (Open WebUI's admin "Add user"), as `user` or
/// `admin` (422 otherwise): the admin chooses the password and passes it on out of band; the user can change it
/// under Account. The password policy applies. Audited `user.added`.
pub async fn add_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(p): Json<AddUser>,
) -> Result<(StatusCode, Json<AddedUser>), ApiError> {
    use crate::accounts::{create_account, normalise_email, AccountError, NewAccount};
    if p.role == crate::accounts::AccountRole::Pending {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "role must be user or admin"));
    }
    let email = normalise_email(&p.email).map_err(|e| api_error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    crate::local_auth::check_password_policy(&p.password).map_err(|r| api_error(StatusCode::UNPROCESSABLE_ENTITY, r))?;
    let hash = crate::local_auth::hash_password(&p.password)
        .await
        .map_err(|e| crate::handlers::internal("hash password", e))?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    if p.role == crate::accounts::AccountRole::Admin {
        crate::admin_bootstrap::lock_admins(&mut tx).await.map_err(db)?;
    }
    let acct = NewAccount { name: Some(&p.name), email: Some(&email), password_hash: Some(&hash), role: p.role };
    let (id, _) = create_account(&mut tx, &acct, &ctx.principal_code, "user.added").await.map_err(|e| match e {
        AccountError::Db(e) => db(e),
        AccountError::EmailTaken => api_error(StatusCode::CONFLICT, e.to_string()),
        other => api_error(StatusCode::UNPROCESSABLE_ENTITY, other.to_string()),
    })?;
    tx.commit().await.map_err(db)?;
    Ok((StatusCode::CREATED, Json(AddedUser { user_id: id })))
}

/// Approve a pending account: it becomes an active `user`, in the given
/// groups. Audited `signup.approved`.
pub async fn approve_user(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(p): Json<Approve>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    if lock_human(&mut tx, id).await? != "PENDING" {
        return Err(api_error(StatusCode::CONFLICT, "user is not pending"));
    }
    sqlx::query("UPDATE principal SET status = 'ACTIVE', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::groups_api::set_user_groups(&mut tx, id, &p.groups).await?;
    audit(&mut tx, &ctx, "signup.approved", id, serde_json::json!({ "groups": p.groups })).await?;
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /admin/users/:id/groups`: put the user in exactly these groups.
pub async fn set_groups(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(groups): Json<Vec<Uuid>>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_human(&mut tx, id).await?;
    crate::groups_api::set_user_groups(&mut tx, id, &groups).await?;
    audit(&mut tx, &ctx, "user.groups_changed", id, serde_json::json!({ "groups": groups })).await?;
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Reject a pending account: it is disabled and signed out everywhere.
/// Audited `signup.rejected`.
pub async fn reject_user(State(state): State<AppState>, Extension(ctx): Extension<AuthContext>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    if lock_human(&mut tx, id).await? != "PENDING" {
        return Err(api_error(StatusCode::CONFLICT, "user is not pending"));
    }
    sqlx::query("UPDATE principal SET status = 'DISABLED', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("UPDATE user_session SET revoked_at = now() WHERE principal_id = $1 AND revoked_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    audit(&mut tx, &ctx, "signup.rejected", id, serde_json::json!({})).await?;
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Cut a user off now: no sign-in, no session, no API key.
/// Keys are kept but stop verifying while the user is not ACTIVE.
pub async fn disable_user(State(state): State<AppState>, Extension(ctx): Extension<AuthContext>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    if lock_human(&mut tx, id).await? == "DISABLED" {
        return Ok(StatusCode::NO_CONTENT);
    }
    refuse_removing_last_admin(&mut tx, id).await?;
    sqlx::query("UPDATE principal SET status = 'DISABLED', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let sessions = sqlx::query("UPDATE user_session SET revoked_at = now() WHERE principal_id = $1 AND revoked_at IS NULL")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
    audit(&mut tx, &ctx, "user.disabled", id, serde_json::json!({ "sessions_revoked": sessions })).await?;
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

/// 409 when `id` is an admin and no other ACTIVE admin would remain. Takes
/// the admin lock, so it holds until the caller's transaction ends.
async fn refuse_removing_last_admin(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<(), ApiError> {
    crate::admin_bootstrap::lock_admins(tx).await.map_err(db)?;
    let is_admin: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principal_admin WHERE principal_id = $1)")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if is_admin && crate::admin_bootstrap::other_active_admins(tx, id).await.map_err(db)? == 0 {
        return Err(api_error(StatusCode::CONFLICT, "this is the last active admin"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SetAdmin {
    pub admin: bool,
}

/// `PUT /admin/users/:id/admin`: grant or remove the admin capability. It is
/// separate from the role: it neither needs one nor gives trading powers.
/// Removing the last ACTIVE admin is refused (409).
pub async fn set_admin(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(p): Json<SetAdmin>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    let status = lock_human(&mut tx, id).await?;
    if p.admin {
        if status == "DISABLED" {
            return Err(api_error(StatusCode::CONFLICT, "a disabled user cannot be made admin"));
        }
        crate::admin_bootstrap::lock_admins(&mut tx).await.map_err(db)?;
        let granted = sqlx::query(
            "INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&ctx.principal_code)
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
        if granted == 0 {
            return Ok(StatusCode::NO_CONTENT);
        }
        audit(&mut tx, &ctx, "admin.granted", id, serde_json::json!({})).await?;
    } else {
        refuse_removing_last_admin(&mut tx, id).await?;
        let removed = sqlx::query("DELETE FROM principal_admin WHERE principal_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?
            .rows_affected();
        if removed == 0 {
            return Ok(StatusCode::NO_CONTENT);
        }
        audit(&mut tx, &ctx, "admin.revoked", id, serde_json::json!({})).await?;
    }
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Re-activate a disabled user who has signed in before or holds an API key.
pub async fn enable_user(State(state): State<AppState>, Extension(ctx): Extension<AuthContext>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    match lock_human(&mut tx, id).await?.as_str() {
        "ACTIVE" => return Ok(StatusCode::NO_CONTENT),
        "INVITED" | "PENDING" => return Err(api_error(StatusCode::CONFLICT, "user is not approved; approve or reject them")),
        _ => {}
    }
    let has_access: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM principal_identity WHERE principal_id = $1) \
             OR EXISTS(SELECT 1 FROM api_key WHERE principal_id = $1 AND revoked_at IS NULL)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    if !has_access {
        return Err(api_error(StatusCode::CONFLICT, "user never signed in; add them again"));
    }
    sqlx::query("UPDATE principal SET status = 'ACTIVE', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_violation(&e, |d| d.is_unique_violation()) {
                api_error(StatusCode::CONFLICT, "another user now holds this email")
            } else {
                db(e)
            }
        })?;
    audit(&mut tx, &ctx, "user.enabled", id, serde_json::json!({})).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_keys(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ApiKeyRecord>>, ApiError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principal WHERE id = $1 AND principal_type = 'HUMAN')")
        .bind(id)
        .fetch_one(state.pool())
        .await
        .map_err(db)?;
    if !exists {
        return Err(api_error(StatusCode::NOT_FOUND, "user not found"));
    }
    let keys = sqlx::query_as::<_, ApiKeyRecord>(
        "SELECT id, principal_id, key_id, name, created_at FROM api_key \
          WHERE principal_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(id)
    .fetch_all(state.pool())
    .await
    .map_err(db)?;
    Ok(Json(keys))
}

/// Mint a key for an ACTIVE user; the secret is in this response only.
pub async fn create_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(p): Json<CreateKey>,
) -> Result<Json<ApiKeyRecord>, ApiError> {
    let material = crate::admin::generate_key_material().await?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    if lock_human(&mut tx, id).await? != "ACTIVE" {
        return Err(api_error(StatusCode::CONFLICT, "only an active user can hold an API key"));
    }
    let mut record = sqlx::query_as::<_, ApiKeyRecord>(
        "INSERT INTO api_key (principal_id, key_id, secret_hash, name) VALUES ($1, $2, $3, $4) \
         RETURNING id, principal_id, key_id, name, created_at",
    )
    .bind(id)
    .bind(&material.key_id)
    .bind(&material.secret_hash)
    .bind(&p.name)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    audit(&mut tx, &ctx, "key.created", id, serde_json::json!({ "key_id": material.key_id, "name": p.name })).await?;
    tx.commit().await.map_err(db)?;
    record.secret = Some(material.plaintext_secret);
    Ok(Json(record))
}

pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, key_id)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_human(&mut tx, id).await?;
    let revoked = sqlx::query(
        "UPDATE api_key SET revoked_at = now() WHERE key_id = $1 AND principal_id = $2 AND revoked_at IS NULL",
    )
    .bind(&key_id)
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(db)?
    .rows_affected();
    if revoked == 0 {
        return Err(api_error(StatusCode::NOT_FOUND, "key not found"));
    }
    audit(&mut tx, &ctx, "key.revoked", id, serde_json::json!({ "key_id": key_id })).await?;
    tx.commit().await.map_err(db)?;
    state.event_bus().publish_auth_changed(id);
    Ok(StatusCode::NO_CONTENT)
}

// ── your own API keys (settings) ───────────────────────────────────────────

/// Only a signed-in person manages their own keys: an API key cannot mint or
/// revoke keys, and creating or revoking one is a mutation behind the MFA gate.
fn own_keys_session(ctx: &AuthContext, mutation: bool) -> Result<(), ApiError> {
    use crate::auth::AuthMethod;
    if !matches!(ctx.auth_method, AuthMethod::Local | AuthMethod::Oidc) {
        return Err(api_error(StatusCode::FORBIDDEN, "sign in to manage your API keys"));
    }
    if mutation {
        crate::authz::require_mfa(ctx)?;
    }
    Ok(())
}

/// `GET /auth/keys`: the signed-in user's active keys.
pub async fn list_own_keys(state: State<AppState>, Extension(ctx): Extension<AuthContext>) -> Result<Json<Vec<ApiKeyRecord>>, ApiError> {
    own_keys_session(&ctx, false)?;
    list_keys(state, Path(ctx.principal_id)).await
}

/// `POST /auth/keys`: a key acting as the signed-in user, with their group
/// access; the secret is in this response only.
pub async fn create_own_key(
    state: State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    body: Json<CreateKey>,
) -> Result<Json<ApiKeyRecord>, ApiError> {
    own_keys_session(&ctx, true)?;
    let id = ctx.principal_id;
    create_key(state, Extension(ctx), Path(id), body).await
}

/// `DELETE /auth/keys/:key_id`: revoke one of the signed-in user's own keys.
pub async fn revoke_own_key(
    state: State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    own_keys_session(&ctx, true)?;
    let id = ctx.principal_id;
    revoke_key(state, Extension(ctx), Path((id, key_id))).await
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SetupStatus {
    pub venue_connected: bool,
    pub portfolio_exists: bool,
    pub sign_in_ok: bool,
    pub user_added: bool,
    pub first_order: bool,
}

/// `GET /admin/setup-status`: which first-run steps are done.
#[utoipa::path(
    get, path = "/admin/setup-status", tag = "admin",
    responses((status = 200, description = "OK", body = SetupStatus)),
    security(("bearer_token" = []))
)]
pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, ApiError> {
    let (venue_connected, portfolio_exists, user_added, first_order): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM broker_connection WHERE status = 'ACTIVE' AND credentials IS NOT NULL), \
                EXISTS (SELECT 1 FROM portfolio), \
                EXISTS (SELECT 1 FROM principal p WHERE p.principal_type = 'HUMAN' \
                         AND NOT EXISTS (SELECT 1 FROM principal_admin a WHERE a.principal_id = p.id)), \
                EXISTS (SELECT 1 FROM order_state)",
    )
    .fetch_one(state.pool())
    .await
    .map_err(db)?;
    let oidc = state.oidc_status();
    Ok(Json(SetupStatus {
        venue_connected,
        portfolio_exists,
        sign_in_ok: oidc.configured && oidc.discovery_ok,
        user_added,
        first_order,
    }))
}

/// `GET /admin/auth/status`: whether OIDC sign-in is on, and why not.
pub async fn auth_status(State(state): State<AppState>) -> Json<crate::app_state::OidcStatus> {
    Json(state.oidc_status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use crate::accounts::AccountRole;
    use crate::handlers::tests::test_app_state;
    const BASE: &str = "https://oms.example.com";

    /// A named admin acting through these handlers, as `admin_middleware`
    /// would inject it.
    fn actor() -> AuthContext {
        AuthContext::machine(Uuid::nil(), "ada".to_string())
    }

    async fn state(pool: &PgPool, base: Option<&str>) -> AppState {
        let mut s = test_app_state(pool.clone());
        s.session_config.public_base_url = base.map(str::to_string);
        s
    }

    async fn portfolio(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')")
            .bind(id)
            .bind(format!("p-{}", id.simple()))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn status_of(pool: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM principal WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
    }

    async fn audit_count(pool: &PgPool, action: &str, target: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1 AND target_principal_id = $2")
            .bind(action)
            .bind(target)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// An ACTIVE signed-in user with a live session and one API key.
    async fn active_user(pool: &PgPool, st: &AppState) -> (Uuid, String, String, String) {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, display_name, status, email) \
             VALUES ($1, $2, 'HUMAN', 'Ann', 'ACTIVE', $3)",
        )
        .bind(id)
        .bind(format!("ann-{}", id.simple()))
        .bind(format!("{}@fundco.com", id.simple()))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'trader')")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        let key = create_key(State(st.clone()), Extension(actor()), Path(id), Json(CreateKey { name: Some("k".into()) }))
            .await
            .unwrap()
            .0;
        let session = crate::sessions::create_session(pool, id, crate::sessions::AuthMethod::Oidc, false, &Default::default(), None).await.unwrap();
        (id, key.key_id, key.secret.unwrap(), session.hash)
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn setup_status_reflects_db_state() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let get = || async { setup_status(State(st.clone())).await.unwrap().0 };

        let s = get().await;
        assert!(!s.venue_connected && !s.portfolio_exists && !s.sign_in_ok && !s.user_added && !s.first_order);

        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment) VALUES ('alpaca-paper', 'ALPACA', 'PAPER')")
            .execute(&pool)
            .await
            .unwrap();
        // A seeded connection without credentials is not a connected venue.
        assert!(!get().await.venue_connected, "a connection without credentials must not count");
        sqlx::query("UPDATE broker_connection SET credentials = '\\x00'::bytea WHERE code = 'alpaca-paper'")
            .execute(&pool).await.unwrap();
        let s = get().await;
        assert!(s.venue_connected && !s.portfolio_exists);

        let portfolio_id = portfolio(&pool).await;
        assert!(get().await.portfolio_exists);

        // Sign-in counts only once discovery has succeeded, not merely configured.
        st.set_oidc_status(crate::app_state::OidcStatus {
            configured: true,
            issuer: Some("https://id.example.com".into()),
            discovery_ok: false,
            error: Some("down".into()),
            redirect_uri: None,
        });
        assert!(!get().await.sign_in_ok);
        st.set_oidc_status(crate::app_state::OidcStatus { discovery_ok: true, error: None, ..st.oidc_status() });
        assert!(get().await.sign_in_ok);

        let added = add(&st, &actor(), &format!("u-{}@fundco.com", Uuid::new_v4().simple()), AccountRole::User).await;
        assert!(get().await.user_added);
        assert!(!get().await.first_order);

        let account_id = Uuid::new_v4();
        sqlx::query("INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) VALUES ($1, 'acc1', 'alpaca-paper', 'acc1', 'ACTIVE')")
            .bind(account_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO order_state (order_id, client_order_id, principal_id, portfolio_id, account_id, \
             broker_connection_code, instrument_id, side, order_type, time_in_force, \
             original_qty, leaves_qty, cum_qty, status, version) \
             VALUES ($1, 'c1', $2, $3, $4, 'alpaca-paper', 'X', 'buy', 'market', 'day', 1, 1, 0, 'submitted', 1)",
        )
        .bind(Uuid::new_v4())
        .bind(added)
        .bind(portfolio_id)
        .bind(account_id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(get().await.first_order);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn auth_status_reports_unconfigured() {
        let pool = migrated_db().await;
        let st = state(&pool, None).await;
        let s = auth_status(State(st)).await.0;
        assert!(!s.configured && !s.discovery_ok);
        assert!(s.issuer.is_none() && s.error.is_none() && s.redirect_uri.is_none());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn disable_kills_sessions_and_keys() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let (id, key_id, secret, hash) = active_user(&pool, &st).await;
        assert!(crate::sessions::lookup_session(&pool, &hash).await.unwrap().is_some());
        assert!(crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().is_some());

        disable_user(State(st.clone()), Extension(actor()), Path(id)).await.unwrap();

        assert_eq!(status_of(&pool, id).await, "DISABLED");
        assert!(crate::sessions::lookup_session(&pool, &hash).await.unwrap().is_none());
        assert!(crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().is_none());
        assert_eq!(audit_count(&pool, "user.disabled", id).await, 1);
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM user_session WHERE principal_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(live, 0);

        enable_user(State(st.clone()), Extension(actor()), Path(id)).await.unwrap();
        assert_eq!(status_of(&pool, id).await, "ACTIVE");
        assert_eq!(audit_count(&pool, "user.enabled", id).await, 1);
        // Enabling does not resurrect the killed session.
        assert!(crate::sessions::lookup_session(&pool, &hash).await.unwrap().is_none());
        assert!(crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().is_some());
    }

    async fn grant_flags(pool: &PgPool, principal_id: Uuid, portfolio_id: Uuid) -> (bool, bool) {
        let row = sqlx::query(
            "SELECT can_trade, can_allocate FROM portfolio_access \
              WHERE principal_id = $1 AND portfolio_id = $2",
        )
        .bind(principal_id)
        .bind(portfolio_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (row.get("can_trade"), row.get("can_allocate"))
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn list_users_excludes_strategies() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let sid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')",
        )
        .bind(sid)
        .bind(format!("strat-{}", sid.simple()))
        .execute(&pool)
        .await
        .unwrap();
        let (human, ..) = active_user(&pool, &st).await;
        // A last sign-in shows as the issuer host.
        sqlx::query(
            "INSERT INTO principal_identity (issuer, subject, principal_id, last_login_at) \
             VALUES ('https://login.example.org/realms/x', 's', $1, now())",
        )
        .bind(human)
        .execute(&pool)
        .await
        .unwrap();

        let users = list_users(State(st.clone())).await.unwrap().0;
        assert!(users.iter().all(|u| u.principal_id != sid));
        let me = users.iter().find(|u| u.principal_id == human).unwrap();
        assert_eq!(me.sign_in.as_deref(), Some("login.example.org"));
        // Strategies are not reachable through the user routes either.
        let err = disable_user(State(st), Extension(actor()), Path(sid)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(status_of(&pool, sid).await, "ACTIVE");
    }

    const PW: &str = "correct horse battery staple";

    async fn add(st: &AppState, ctx: &AuthContext, email: &str, role: AccountRole) -> Uuid {
        let body = AddUser { name: "Bo".into(), email: email.into(), password: PW.into(), role };
        let (status, out) = add_user(State(st.clone()), Extension(ctx.clone()), Json(body)).await.unwrap();
        assert_eq!(status, StatusCode::CREATED);
        out.0.user_id
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn add_user_creates_local_account_with_password() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let email = format!("bo-{}@fundco.com", Uuid::new_v4().simple());
        let id = add(&st, &actor(), &email, AccountRole::User).await;
        let row = fetch_users(&pool, Some(id)).await.unwrap().pop().unwrap();
        assert_eq!((row.status.as_str(), row.account_role, row.admin), ("ACTIVE", "user", false));
        let hash: String = sqlx::query_scalar("SELECT password_hash FROM local_credential WHERE principal_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(crate::local_auth::verify_password(&hash, PW).await);
        assert_eq!(audit_count(&pool, "user.added", id).await, 1);

        // Keys work for it, as for anyone.
        let key = create_key(State(st.clone()), Extension(actor()), Path(id), Json(CreateKey { name: Some("ci".into()) }))
            .await
            .unwrap()
            .0;
        assert!(crate::auth::verify_key(&pool, &key.key_id, key.secret.as_deref().unwrap()).await.unwrap().is_some());

        let admin_id = add(&st, &actor(), &format!("ad-{}@fundco.com", Uuid::new_v4().simple()), AccountRole::Admin).await;
        assert!(fetch_users(&pool, Some(admin_id)).await.unwrap()[0].admin);
        let pending = add_user(State(st.clone()), Extension(actor()),
            Json(AddUser { name: "x".into(), email: "pe@fundco.com".into(), password: PW.into(), role: AccountRole::Pending })).await.unwrap_err();
        assert_eq!(pending.status, StatusCode::UNPROCESSABLE_ENTITY, "pending is for sign-ups, not added users");

        let dup = add_user(State(st.clone()), Extension(actor()),
            Json(AddUser { name: "x".into(), email: email.to_uppercase(), password: PW.into(), role: AccountRole::User })).await.unwrap_err();
        assert_eq!(dup.status, StatusCode::CONFLICT);
        let weak = add_user(State(st), Extension(actor()),
            Json(AddUser { name: "x".into(), email: "w@fundco.com".into(), password: "short".into(), role: AccountRole::User })).await.unwrap_err();
        assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// A self-signup waiting for approval.
    async fn pending_user(pool: &PgPool) -> Uuid {
        let email = format!("p-{}@fundco.com", Uuid::new_v4().simple());
        let policy = crate::accounts::SignupPolicy { enabled: true, pending_by_default: true };
        // The first signup on a fresh database would be the admin.
        add(&test_app_state(pool.clone()), &actor(), &format!("a-{}@fundco.com", Uuid::new_v4().simple()), AccountRole::Admin).await;
        crate::accounts::signup(pool, "P", &email, "x", policy).await.unwrap().principal_id
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn approve_pending_sets_user_and_portfolio_access() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let id = pending_user(&pool).await;
        let pf = portfolio(&pool).await;
        let g = Uuid::new_v4();
        sqlx::query("INSERT INTO user_group (id, name) VALUES ($1, 'desk')").bind(g).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, 'trade')").bind(g).bind(pf).execute(&pool).await.unwrap();
        let body = Approve { groups: vec![g] };
        assert_eq!(approve_user(State(st.clone()), Extension(actor()), Path(id), Json(body)).await.unwrap(), StatusCode::NO_CONTENT);
        let row = fetch_users(&pool, Some(id)).await.unwrap().pop().unwrap();
        assert_eq!((row.status.as_str(), row.account_role), ("ACTIVE", "user"));
        assert_eq!(row.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["desk"]);
        assert_eq!((row.portfolios.len(), row.portfolios[0].access), (1, "trade"));
        assert_eq!(grant_flags(&pool, id, pf).await, (true, true));
        assert_eq!(audit_count(&pool, "signup.approved", id).await, 1);
        let again = approve_user(State(st.clone()), Extension(actor()), Path(id), Json(Approve::default())).await.unwrap_err();
        assert_eq!(again.status, StatusCode::CONFLICT, "only a pending account can be approved");

        let other = pending_user(&pool).await;
        reject_user(State(st.clone()), Extension(actor()), Path(other)).await.unwrap();
        assert_eq!(status_of(&pool, other).await, "DISABLED");
        assert_eq!(audit_count(&pool, "signup.rejected", other).await, 1);
    }

    /// An ACTIVE admin (no role) acting as themselves.
    async fn admin(pool: &PgPool) -> AuthContext {
        let id = Uuid::new_v4();
        let code = format!("adm-{}", id.simple());
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(id)
            .bind(&code)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, 'test')")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        AuthContext::machine(id, code)
    }

    async fn is_admin(pool: &PgPool, id: Uuid) -> bool {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principal_admin WHERE principal_id = $1)")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn put_admin(st: &AppState, ctx: &AuthContext, id: Uuid, admin: bool) -> impl std::future::Future<Output = Result<StatusCode, ApiError>> {
        set_admin(State(st.clone()), Extension(ctx.clone()), Path(id), Json(SetAdmin { admin }))
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn last_admin_cannot_be_removed() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let ada = admin(&pool).await;

        let err = put_admin(&st, &ada, ada.principal_id, false).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert!(is_admin(&pool, ada.principal_id).await);

        // Granting needs no role and gives none.
        let (bob, ..) = active_user(&pool, &st).await;
        assert_eq!(put_admin(&st, &ada, bob, true).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(is_admin(&pool, bob).await);
        assert_eq!(audit_count(&pool, "admin.granted", bob).await, 1);

        assert_eq!(put_admin(&st, &ada, ada.principal_id, false).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(!is_admin(&pool, ada.principal_id).await);
        assert_eq!(audit_count(&pool, "admin.revoked", ada.principal_id).await, 1);
        let bob_ctx = AuthContext::machine(bob, "bob".into());
        assert_eq!(put_admin(&st, &bob_ctx, bob, false).await.unwrap_err().status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn last_admin_cannot_disable_self() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let ada = admin(&pool).await;

        let err = disable_user(State(st.clone()), Extension(ada.clone()), Path(ada.principal_id)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(status_of(&pool, ada.principal_id).await, "ACTIVE");

        // With a second active admin it goes through.
        admin(&pool).await;
        disable_user(State(st.clone()), Extension(ada.clone()), Path(ada.principal_id)).await.unwrap();
        assert_eq!(status_of(&pool, ada.principal_id).await, "DISABLED");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn audit_actor_is_named_admin() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let ada = admin(&pool).await;
        let (id, ..) = active_user(&pool, &st).await;

        set_groups(State(st.clone()), Extension(ada.clone()), Path(id), Json(vec![])).await.unwrap();
        let added = add(&st, &ada, &format!("{}@fundco.com", Uuid::new_v4().simple()), AccountRole::User).await;

        for (action, target) in [("user.groups_changed", id), ("user.added", added)] {
            let actor: String = sqlx::query_scalar("SELECT actor FROM audit_event WHERE action = $1 AND target_principal_id = $2")
                .bind(action)
                .bind(target)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(actor, ada.principal_code, "{action}");
        }
    }
}
