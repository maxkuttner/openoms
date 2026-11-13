//! Admin API for users: invite, role, portfolio access, disable and API keys.
//! Admin-token routes (`/admin/users*`), HUMAN principals only; strategies
//! keep their own admin surface. Every mutation writes an `audit_event`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::admin::{ApiKeyRecord, CreateKey};
use crate::app_state::AppState;
use crate::handlers::ApiError;
use crate::invites::{self, InviteError, NewInvite};

#[derive(Debug, Serialize, Deserialize)]
pub struct PortfolioAccess {
    pub portfolio_id: Uuid,
    pub can_trade: bool,
}

#[derive(Debug, Serialize)]
pub struct UserPortfolio {
    pub portfolio_id: Uuid,
    pub code: String,
    pub can_trade: bool,
}

#[derive(Debug, Serialize)]
pub struct PendingInvite {
    pub invite_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct UserRow {
    pub principal_id: Uuid,
    pub code: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub role: Option<String>,
    pub portfolios: Vec<UserPortfolio>,
    /// Host of the issuer the user last signed in with.
    pub sign_in: Option<String>,
    pub status: String,
    pub invite: Option<PendingInvite>,
}

#[derive(Debug, Deserialize)]
pub struct InviteRequest {
    pub email: String,
    pub role: String,
    #[serde(default)]
    pub portfolios: Vec<PortfolioAccess>,
}

#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub invite_id: Uuid,
    /// The invited user, so the caller can open them straight away.
    pub user_id: Uuid,
    pub link: String,
    pub expires_at: DateTime<Utc>,
}

/// An API-key-only user: no sign-in, access through keys alone.
#[derive(Debug, Deserialize)]
pub struct CreateKeyOnlyUser {
    pub code: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct SetRole {
    pub role: String,
}

/// Actor recorded on audit rows: these routes sit behind the admin token, which
/// identifies no individual.
const ACTOR: &str = "admin";

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn db(err: sqlx::Error) -> ApiError {
    crate::handlers::internal("database error", err)
}

fn is_violation(err: &sqlx::Error, check: impl Fn(&dyn sqlx::error::DatabaseError) -> bool) -> bool {
    matches!(err, sqlx::Error::Database(d) if check(d.as_ref()))
}

fn invite_error(err: InviteError) -> ApiError {
    match err {
        InviteError::AlreadyInvited | InviteError::AlreadyUser | InviteError::NotLinkable => {
            api_error(StatusCode::CONFLICT, err.to_string())
        }
        InviteError::BadRole => api_error(StatusCode::UNPROCESSABLE_ENTITY, err.to_string()),
        InviteError::NotFound => api_error(StatusCode::NOT_FOUND, err.to_string()),
        InviteError::Db(err) => db(err),
    }
}

/// The configured public origin that invite links point at.
fn public_base_url(state: &AppState) -> Result<&str, ApiError> {
    state
        .session_config
        .public_base_url
        .as_deref()
        .map(|u| u.trim_end_matches('/'))
        .filter(|u| !u.is_empty())
        .ok_or_else(|| api_error(StatusCode::UNPROCESSABLE_ENTITY, "Set public_base_url before inviting"))
}

fn invite_response(base: &str, invite: NewInvite) -> InviteResponse {
    InviteResponse {
        link: format!("{base}/auth/invite/{}", invite.token),
        invite_id: invite.invite_id,
        user_id: invite.principal_id,
        expires_at: invite.expires_at,
    }
}

fn require_role(role: &str) -> Result<(), ApiError> {
    if invites::is_assignable_role(role) {
        Ok(())
    } else {
        Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "role must be pm, trader or viewer"))
    }
}

/// `https://login.example.org/realms/x` -> `login.example.org`.
fn issuer_host(issuer: &str) -> String {
    let rest = issuer.split_once("://").map_or(issuer, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    action: &str,
    target: Uuid,
    detail: serde_json::Value,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO audit_event (actor, action, target_principal_id, detail) VALUES ($1, $2, $3, $4)")
        .bind(ACTOR)
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
        "SELECT p.id, p.code, p.display_name, p.email, p.status, r.role_code, \
                (SELECT i.issuer FROM principal_identity i \
                  WHERE i.principal_id = p.id AND i.last_login_at IS NOT NULL \
                  ORDER BY i.last_login_at DESC LIMIT 1) AS issuer, \
                v.id AS invite_id, v.expires_at \
           FROM principal p \
           LEFT JOIN principal_role r ON r.principal_id = p.id \
           LEFT JOIN LATERAL ( \
                SELECT id, expires_at FROM invite \
                 WHERE principal_id = p.id AND accepted_at IS NULL AND revoked_at IS NULL \
                 ORDER BY created_at DESC LIMIT 1) v ON true \
          WHERE p.principal_type = 'HUMAN' AND ($1::uuid IS NULL OR p.id = $1) \
          ORDER BY p.created_at DESC, p.code",
    )
    .bind(only)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let grants = sqlx::query(
        "SELECT g.principal_id, g.portfolio_id, pf.code, g.can_trade \
           FROM principal_portfolio_grant g JOIN portfolio pf ON pf.id = g.portfolio_id \
          WHERE g.can_view AND ($1::uuid IS NULL OR g.principal_id = $1) \
          ORDER BY pf.code",
    )
    .bind(only)
    .fetch_all(pool)
    .await
    .map_err(db)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.get("id");
            let invite_id: Option<Uuid> = r.get("invite_id");
            UserRow {
                principal_id: id,
                code: r.get("code"),
                display_name: r.get("display_name"),
                email: r.get("email"),
                role: r.get("role_code"),
                portfolios: grants
                    .iter()
                    .filter(|g| g.get::<Uuid, _>("principal_id") == id)
                    .map(|g| UserPortfolio {
                        portfolio_id: g.get("portfolio_id"),
                        code: g.get("code"),
                        can_trade: g.get("can_trade"),
                    })
                    .collect(),
                sign_in: r.get::<Option<String>, _>("issuer").map(|i| issuer_host(&i)),
                status: r.get("status"),
                invite: invite_id.map(|invite_id| PendingInvite { invite_id, expires_at: r.get("expires_at") }),
            }
        })
        .collect())
}

pub async fn list_users(State(state): State<AppState>) -> Result<Json<Vec<UserRow>>, ApiError> {
    Ok(Json(fetch_users(state.pool(), None).await?))
}

pub async fn create_key_only_user(
    State(state): State<AppState>,
    Json(p): Json<CreateKeyOnlyUser>,
) -> Result<Json<UserRow>, ApiError> {
    require_role(&p.role)?;
    let code = p.code.trim();
    if code.is_empty() {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "code is required"));
    }
    let id = Uuid::new_v4();
    let mut tx = state.pool().begin().await.map_err(db)?;
    sqlx::query(
        "INSERT INTO principal (id, code, principal_type, display_name, status) \
         VALUES ($1, $2, 'HUMAN', $3, 'ACTIVE')",
    )
    .bind(id)
    .bind(code)
    .bind(p.display_name.trim())
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if is_violation(&e, |d| d.is_unique_violation()) {
            api_error(StatusCode::CONFLICT, "a user with this code already exists")
        } else {
            db(e)
        }
    })?;
    sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
        .bind(id)
        .bind(&p.role)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    audit(&mut tx, "user.created", id, serde_json::json!({ "code": code, "role": p.role, "key_only": true })).await?;
    tx.commit().await.map_err(db)?;
    let user = fetch_users(state.pool(), Some(id)).await?.pop();
    user.map(Json).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "user not found"))
}

pub async fn invite_user(
    State(state): State<AppState>,
    Json(p): Json<InviteRequest>,
) -> Result<Json<InviteResponse>, ApiError> {
    let base = public_base_url(&state)?;
    require_role(&p.role)?;
    let mut ids: Vec<Uuid> = p.portfolios.iter().map(|g| g.portfolio_id).collect();
    ids.sort();
    ids.dedup();
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio WHERE id = ANY($1)")
        .bind(&ids)
        .fetch_one(state.pool())
        .await
        .map_err(db)?;
    if known != ids.len() as i64 || ids.len() != p.portfolios.len() {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown or repeated portfolio"));
    }
    let grants: Vec<(Uuid, bool)> = p.portfolios.iter().map(|g| (g.portfolio_id, g.can_trade)).collect();
    let invite = invites::create(state.pool(), &p.email, &p.role, &grants, ACTOR).await.map_err(invite_error)?;
    Ok(Json(invite_response(base, invite)))
}

pub async fn revoke_invite(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    invites::revoke(state.pool(), id, ACTOR).await.map_err(invite_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reissue_invite(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<InviteResponse>, ApiError> {
    let base = public_base_url(&state)?;
    let invite = invites::reissue(state.pool(), id, ACTOR).await.map_err(invite_error)?;
    Ok(Json(invite_response(base, invite)))
}

/// A link for an active user to add a second sign-in provider.
pub async fn link_invite(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<InviteResponse>, ApiError> {
    let base = public_base_url(&state)?;
    let invite = invites::create_link(state.pool(), id, ACTOR).await.map_err(invite_error)?;
    Ok(Json(invite_response(base, invite)))
}

pub async fn set_role(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(p): Json<SetRole>,
) -> Result<StatusCode, ApiError> {
    require_role(&p.role)?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_human(&mut tx, id).await?;
    let from: Option<String> = sqlx::query_scalar("SELECT role_code FROM principal_role WHERE principal_id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query(
        "INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2) \
         ON CONFLICT (principal_id) DO UPDATE SET role_code = EXCLUDED.role_code",
    )
    .bind(id)
    .bind(&p.role)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    audit(&mut tx, "user.role_changed", id, serde_json::json!({ "from": from, "to": p.role })).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Replace the user's portfolio access with exactly `grants`.
pub async fn set_portfolios(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(grants): Json<Vec<PortfolioAccess>>,
) -> Result<StatusCode, ApiError> {
    let ids: Vec<Uuid> = grants.iter().map(|g| g.portfolio_id).collect();
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_human(&mut tx, id).await?;
    sqlx::query("DELETE FROM principal_portfolio_grant WHERE principal_id = $1 AND NOT (portfolio_id = ANY($2))")
        .bind(id)
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    for g in &grants {
        sqlx::query(
            // The trade toggle also controls allocation out of the portfolio.
            "INSERT INTO principal_portfolio_grant \
                (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
             VALUES ($1, $2, $3, $4, true, $4) \
             ON CONFLICT (principal_id, portfolio_id) \
             DO UPDATE SET can_trade = EXCLUDED.can_trade, can_allocate = EXCLUDED.can_allocate, \
                           can_view = true, updated_at = now()",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(g.portfolio_id)
        .bind(g.can_trade)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_violation(&e, |d| d.is_foreign_key_violation()) {
                api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown portfolio")
            } else {
                db(e)
            }
        })?;
    }
    audit(&mut tx, "user.portfolios_changed", id, serde_json::json!({ "portfolios": grants })).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Cut a user off now: no sign-in, no session, no API key, no pending invite.
/// Keys are kept but stop verifying while the user is not ACTIVE.
pub async fn disable_user(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    if lock_human(&mut tx, id).await? == "DISABLED" {
        return Ok(StatusCode::NO_CONTENT);
    }
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
    sqlx::query(
        "UPDATE invite SET revoked_at = now() \
          WHERE principal_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    audit(&mut tx, "user.disabled", id, serde_json::json!({ "sessions_revoked": sessions })).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Re-activate a disabled user who has signed in before or holds an API key.
/// One who never got in has to be invited again.
pub async fn enable_user(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    match lock_human(&mut tx, id).await?.as_str() {
        "ACTIVE" => return Ok(StatusCode::NO_CONTENT),
        "INVITED" => return Err(api_error(StatusCode::CONFLICT, "user has a pending invite")),
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
        return Err(api_error(StatusCode::CONFLICT, "user never signed in; reissue the invite instead"));
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
    audit(&mut tx, "user.enabled", id, serde_json::json!({})).await?;
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
    audit(&mut tx, "key.created", id, serde_json::json!({ "key_id": material.key_id, "name": p.name })).await?;
    tx.commit().await.map_err(db)?;
    record.secret = Some(material.plaintext_secret);
    Ok(Json(record))
}

pub async fn revoke_key(
    State(state): State<AppState>,
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
    audit(&mut tx, "key.revoked", id, serde_json::json!({ "key_id": key_id })).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub venue_connected: bool,
    pub portfolio_exists: bool,
    pub sign_in_ok: bool,
    pub user_invited: bool,
    pub first_order: bool,
}

/// `GET /admin/setup-status`: which first-run steps are done.
pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, ApiError> {
    let (venue_connected, portfolio_exists, user_invited, first_order): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM broker_connection WHERE status = 'ACTIVE'), \
                EXISTS (SELECT 1 FROM portfolio), \
                EXISTS (SELECT 1 FROM invite), \
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
        user_invited,
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
    use crate::handlers::tests::test_app_state;
    const BASE: &str = "https://oms.example.com";

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
        let key = create_key(State(st.clone()), Path(id), Json(CreateKey { name: Some("k".into()) }))
            .await
            .unwrap()
            .0;
        let session = crate::sessions::create_session(pool, id, &Default::default(), None).await.unwrap();
        (id, key.key_id, key.secret.unwrap(), session.hash)
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn setup_status_reflects_db_state() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let get = || async { setup_status(State(st.clone())).await.unwrap().0 };

        let s = get().await;
        assert!(!s.venue_connected && !s.portfolio_exists && !s.sign_in_ok && !s.user_invited && !s.first_order);

        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment) VALUES ('alpaca-paper', 'ALPACA', 'PAPER')")
            .execute(&pool)
            .await
            .unwrap();
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

        let email = format!("inv-{}@fundco.com", Uuid::new_v4().simple());
        let invite = crate::invites::create(&pool, &email, "viewer", &[], "admin").await.unwrap();
        assert!(get().await.user_invited);
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
        .bind(invite.principal_id)
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

        disable_user(State(st.clone()), Path(id)).await.unwrap();

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

        enable_user(State(st.clone()), Path(id)).await.unwrap();
        assert_eq!(status_of(&pool, id).await, "ACTIVE");
        assert_eq!(audit_count(&pool, "user.enabled", id).await, 1);
        // Enabling does not resurrect the killed session.
        assert!(crate::sessions::lookup_session(&pool, &hash).await.unwrap().is_none());
        assert!(crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invited_user_api_key_is_refused() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let email = format!("inv-{}@fundco.com", Uuid::new_v4().simple());
        let invite = crate::invites::create(&pool, &email, "viewer", &[], "admin").await.unwrap();

        let err = create_key(State(st), Path(invite.principal_id), Json(CreateKey { name: None }))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM api_key WHERE principal_id = $1")
            .bind(invite.principal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(keys, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invite_endpoint_returns_link_with_public_base_url() {
        let pool = migrated_db().await;
        let p = portfolio(&pool).await;
        let st = state(&pool, Some("https://oms.example.com/")).await;
        let email = format!("new-{}@fundco.com", Uuid::new_v4().simple());

        let out = invite_user(
            State(st.clone()),
            Json(InviteRequest {
                email: email.clone(),
                role: "trader".into(),
                portfolios: vec![PortfolioAccess { portfolio_id: p, can_trade: true }],
            }),
        )
        .await
        .unwrap()
        .0;
        let token = out.link.strip_prefix("https://oms.example.com/auth/invite/").expect("link shape");
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM invite WHERE id = $1")
            .bind(out.invite_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, crate::sessions::hash_session_token(token));
        // The trade toggle also controls allocation.
        assert_eq!(grant_flags(&pool, out.user_id, p).await, (true, true));

        // Duplicate email -> 409; bad role -> 422.
        let dup = invite_user(
            State(st.clone()),
            Json(InviteRequest { email: email.clone(), role: "viewer".into(), portfolios: vec![] }),
        )
        .await
        .unwrap_err();
        assert_eq!(dup.status, StatusCode::CONFLICT);
        let bad = invite_user(
            State(st.clone()),
            Json(InviteRequest { email: "x@fundco.com".into(), role: "strategy".into(), portfolios: vec![] }),
        )
        .await
        .unwrap_err();
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);

        // The row shows the pending invite, and reissue/revoke work through the API.
        let users = list_users(State(st.clone())).await.unwrap().0;
        let row = users.iter().find(|u| u.email.as_deref() == Some(email.as_str())).unwrap();
        assert_eq!(row.status, "INVITED");
        assert_eq!(row.invite.as_ref().unwrap().invite_id, out.invite_id);
        let again = reissue_invite(State(st.clone()), Path(out.invite_id)).await.unwrap().0;
        assert_ne!(again.link, out.link);
        revoke_invite(State(st.clone()), Path(again.invite_id)).await.unwrap();
        assert_eq!(status_of(&pool, row.principal_id).await, "DISABLED");
        let gone = revoke_invite(State(st.clone()), Path(again.invite_id)).await.unwrap_err();
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invite_without_public_base_url_creates_nothing() {
        let pool = migrated_db().await;
        let st = state(&pool, None).await;
        let email = format!("nobase-{}@fundco.com", Uuid::new_v4().simple());
        let err = invite_user(
            State(st),
            Json(InviteRequest { email: email.clone(), role: "viewer".into(), portfolios: vec![] }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(err.message.contains("public_base_url"));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM principal WHERE email = $1")
            .bind(&email)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn link_invite_returns_link_for_active_user() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let (id, ..) = active_user(&pool, &st).await;
        let out = link_invite(State(st.clone()), Path(id)).await.unwrap().0;
        assert!(out.link.starts_with("https://oms.example.com/auth/invite/"));
        let missing = link_invite(State(st), Path(Uuid::new_v4())).await.unwrap_err();
        assert_eq!(missing.status, StatusCode::CONFLICT);
    }

    async fn grant_flags(pool: &PgPool, principal_id: Uuid, portfolio_id: Uuid) -> (bool, bool) {
        let row = sqlx::query(
            "SELECT can_trade, can_allocate FROM principal_portfolio_grant \
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
    async fn set_portfolios_sets_can_allocate_with_can_trade() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let (id, ..) = active_user(&pool, &st).await;
        let p = portfolio(&pool).await;
        for trade in [true, false, true] {
            set_portfolios(State(st.clone()), Path(id), Json(vec![PortfolioAccess { portfolio_id: p, can_trade: trade }]))
                .await
                .unwrap();
            assert_eq!(grant_flags(&pool, id, p).await, (trade, trade));
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn role_change_is_audited() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let (id, ..) = active_user(&pool, &st).await;
        let p = portfolio(&pool).await;

        set_role(State(st.clone()), Path(id), Json(SetRole { role: "viewer".into() })).await.unwrap();
        let role: String = sqlx::query_scalar("SELECT role_code FROM principal_role WHERE principal_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(role, "viewer");
        let detail: serde_json::Value = sqlx::query_scalar(
            "SELECT detail FROM audit_event WHERE action = 'user.role_changed' AND target_principal_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(detail["from"], "trader");
        assert_eq!(detail["to"], "viewer");

        let bad = set_role(State(st.clone()), Path(id), Json(SetRole { role: "strategy".into() }))
            .await
            .unwrap_err();
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        let none = set_role(State(st.clone()), Path(Uuid::new_v4()), Json(SetRole { role: "pm".into() }))
            .await
            .unwrap_err();
        assert_eq!(none.status, StatusCode::NOT_FOUND);

        // Portfolio access is replaced wholesale and audited.
        set_portfolios(State(st.clone()), Path(id), Json(vec![PortfolioAccess { portfolio_id: p, can_trade: true }]))
            .await
            .unwrap();
        set_portfolios(State(st.clone()), Path(id), Json(vec![PortfolioAccess { portfolio_id: p, can_trade: false }]))
            .await
            .unwrap();
        let rows = list_users(State(st.clone())).await.unwrap().0;
        let me = rows.iter().find(|u| u.principal_id == id).unwrap();
        assert_eq!(me.portfolios.len(), 1);
        assert!(!me.portfolios[0].can_trade);
        assert_eq!(audit_count(&pool, "user.portfolios_changed", id).await, 2);
        let unknown = set_portfolios(
            State(st),
            Path(id),
            Json(vec![PortfolioAccess { portfolio_id: Uuid::new_v4(), can_trade: false }]),
        )
        .await
        .unwrap_err();
        assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
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
        assert_eq!(me.role.as_deref(), Some("trader"));
        assert_eq!(me.sign_in.as_deref(), Some("login.example.org"));
        // Strategies are not reachable through the user routes either.
        let err = disable_user(State(st), Path(sid)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(status_of(&pool, sid).await, "ACTIVE");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn key_only_user_can_hold_api_key() {
        let pool = migrated_db().await;
        let st = state(&pool, Some(BASE)).await;
        let code = format!("bot-{}", Uuid::new_v4().simple());
        let row = create_key_only_user(
            State(st.clone()),
            Json(CreateKeyOnlyUser { code: code.clone(), display_name: "Bot".into(), role: "trader".into() }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(row.status, "ACTIVE");
        assert_eq!(row.role.as_deref(), Some("trader"));
        assert_eq!(row.sign_in, None);
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_identity WHERE principal_id = $1")
            .bind(row.principal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 0);

        let key = create_key(State(st.clone()), Path(row.principal_id), Json(CreateKey { name: Some("ci".into()) }))
            .await
            .unwrap()
            .0;
        let (pid, pcode) = crate::auth::verify_key(&pool, &key.key_id, key.secret.as_deref().unwrap())
            .await
            .unwrap()
            .expect("key works");
        assert_eq!((pid, pcode), (row.principal_id, code.clone()));
        assert_eq!(list_keys(State(st.clone()), Path(row.principal_id)).await.unwrap().0.len(), 1);

        revoke_key(State(st.clone()), Path((row.principal_id, key.key_id.clone()))).await.unwrap();
        assert!(crate::auth::verify_key(&pool, &key.key_id, key.secret.as_deref().unwrap()).await.unwrap().is_none());
        assert_eq!(audit_count(&pool, "key.created", row.principal_id).await, 1);
        assert_eq!(audit_count(&pool, "key.revoked", row.principal_id).await, 1);

        let dup = create_key_only_user(
            State(st.clone()),
            Json(CreateKeyOnlyUser { code, display_name: "Bot".into(), role: "trader".into() }),
        )
        .await
        .unwrap_err();
        assert_eq!(dup.status, StatusCode::CONFLICT);
        let bad = create_key_only_user(
            State(st),
            Json(CreateKeyOnlyUser { code: "x".into(), display_name: "x".into(), role: "strategy".into() }),
        )
        .await
        .unwrap_err();
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
