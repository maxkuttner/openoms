//! Trader-created reported strategies: `/strategies/reported*`. Creating one
//! makes its own portfolio, a private group (the caller, `manage`) bound to
//! the strategy, and its first reporting token.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::token;
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::{internal, ApiError};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateReported {
    pub code: String,
    pub name: String,
    pub base_currency: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReportedCreated {
    pub code: String,
    pub portfolio_id: Uuid,
    pub group_id: Uuid,
    /// The reporting bearer token. Shown once.
    pub token: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TokenCreated {
    /// The reporting bearer token. Shown once.
    pub token: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ReportedStrategy {
    pub code: String,
    pub name: String,
    pub portfolio_id: Uuid,
    pub owner_principal_id: Uuid,
    pub state: String,
    pub stale_after_secs: i32,
    pub created_at: DateTime<Utc>,
    /// Set once archived: no reporting, kept for audit.
    pub archived_at: Option<DateTime<Utc>>,
}

fn db(e: sqlx::Error) -> ApiError {
    if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
        return ApiError { status: StatusCode::CONFLICT, message: "a strategy with that code already exists".into() };
    }
    internal("database error", e)
}

fn status(status: StatusCode, message: &str) -> ApiError {
    ApiError { status, message: message.into() }
}

async fn is_admin(pool: &PgPool, id: Uuid) -> Result<bool, ApiError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principal_admin WHERE principal_id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(db)
}

/// 404 unless `code` is a reported strategy; 403 unless the caller owns it or is admin.
async fn require_owner(pool: &PgPool, caller: Uuid, code: &str) -> Result<(), ApiError> {
    let owner: Uuid = sqlx::query_scalar("SELECT owner_principal_id FROM strategy WHERE code = $1 AND mode = 'REPORTED'")
        .bind(code)
        .fetch_optional(pool)
        .await
        .map_err(db)?
        .ok_or_else(|| status(StatusCode::NOT_FOUND, "reported strategy not found"))?;
    if owner == caller || is_admin(pool, caller).await? {
        Ok(())
    } else {
        Err(status(StatusCode::FORBIDDEN, "not authorized"))
    }
}

/// Any person may create a reported strategy; it gets its own portfolio.
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(p): Json<CreateReported>,
) -> Result<Json<ReportedCreated>, ApiError> {
    crate::authz::require_mfa(&auth)?;
    crate::authz::require_grant(state.pool(), &auth, crate::access_grants::GrantKey::Strategies).await?;
    let code = p.code.trim().to_string();
    let name = p.name.trim().to_string();
    if code.is_empty() || name.is_empty() {
        return Err(status(StatusCode::UNPROCESSABLE_ENTITY, "code and name are required"));
    }
    let pool = state.pool();
    let human: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal WHERE id = $1 AND principal_type = 'HUMAN' AND status = 'ACTIVE')",
    )
    .bind(auth.principal_id)
    .fetch_one(pool)
    .await
    .map_err(db)?;
    if !human {
        return Err(status(StatusCode::FORBIDDEN, "only a person can create a reported strategy"));
    }

    let mut tx = pool.begin().await.map_err(db)?;
    let portfolio_id = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status, base_currency) VALUES ($1, $2, $3, 'ACTIVE', $4)")
        .bind(portfolio_id)
        .bind(format!("RPT-{code}"))
        .bind(&name)
        .bind(&p.base_currency)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query(
        "INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, $2, $3, $4, 'REPORTED')",
    )
    .bind(&code)
    .bind(&name)
    .bind(portfolio_id)
    .bind(auth.principal_id)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let group_id = Uuid::new_v4();
    sqlx::query("INSERT INTO user_group (id, name, strategy_code) VALUES ($1, $2, $3)")
        .bind(group_id)
        .bind(format!("strategy {code}"))
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)")
        .bind(group_id)
        .bind(auth.principal_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, 'manage')")
        .bind(group_id)
        .bind(portfolio_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    // The owner's group manages the portfolio: give it the manage-level
    // grants migration 0075 gave every existing manage group.
    crate::access_grants::set_group_grants(
        &mut tx,
        &auth,
        group_id,
        &[crate::access_grants::GrantKey::CancelAny, crate::access_grants::GrantKey::LimitsSet],
    )
    .await?;
    let token = token::issue(&mut tx, &code).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(ReportedCreated { code, portfolio_id, group_id, token }))
}

/// New token; the old active one keeps working for 24 h. At most one token
/// is in grace: rotating again revokes the graced one.
pub async fn rotate_token(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
) -> Result<Json<TokenCreated>, ApiError> {
    crate::authz::require_mfa(&auth)?;
    crate::authz::require_grant(state.pool(), &auth, crate::access_grants::GrantKey::StrategyTokens).await?;
    require_owner(state.pool(), auth.principal_id, &code).await?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    let archived: bool = sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM strategy WHERE code = $1 FOR UPDATE")
        .bind(&code)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if archived {
        return Err(status(StatusCode::CONFLICT, "strategy is archived"));
    }
    sqlx::query(
        "UPDATE strategy_reporting_token SET revoked_at = now() \
          WHERE strategy_code = $1 AND revoked_at IS NULL AND expires_at IS NOT NULL",
    )
    .bind(&code)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    sqlx::query(
        "UPDATE strategy_reporting_token SET expires_at = now() + interval '24 hours' \
          WHERE strategy_code = $1 AND revoked_at IS NULL AND expires_at IS NULL",
    )
    .bind(&code)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let token = token::issue(&mut tx, &code).await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(TokenCreated { token }))
}

/// Revoke every token of the strategy at once.
pub async fn revoke_token(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
) -> Result<StatusCode, ApiError> {
    crate::authz::require_mfa(&auth)?;
    crate::authz::require_grant(state.pool(), &auth, crate::access_grants::GrantKey::StrategyTokens).await?;
    require_owner(state.pool(), auth.principal_id, &code).await?;
    sqlx::query("UPDATE strategy_reporting_token SET revoked_at = now() WHERE strategy_code = $1 AND revoked_at IS NULL")
        .bind(&code)
        .execute(state.pool())
        .await
        .map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Archive: revoke every token, hide the strategy from the list and close its
/// portfolio. Fills, batches and the portfolio's rows stay. Idempotent.
pub async fn archive(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
) -> Result<StatusCode, ApiError> {
    crate::authz::require_mfa(&auth)?;
    crate::authz::require_grant(state.pool(), &auth, crate::access_grants::GrantKey::StrategyTokens).await?;
    require_owner(state.pool(), auth.principal_id, &code).await?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    sqlx::query("UPDATE strategy SET archived_at = COALESCE(archived_at, now()) WHERE code = $1")
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    // Close its portfolio too, so it drops out of every trader-facing portfolio list.
    sqlx::query("UPDATE portfolio SET status = 'CLOSED' WHERE id = (SELECT portfolio_id FROM strategy WHERE code = $1)")
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("UPDATE strategy_reporting_token SET revoked_at = now() WHERE strategy_code = $1 AND revoked_at IS NULL")
        .bind(&code)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's own reported strategies, archived ones included (the UI
/// hides them by default); an admin sees all.
pub async fn list_mine(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<ReportedStrategy>>, ApiError> {
    let admin = is_admin(state.pool(), auth.principal_id).await?;
    let rows = sqlx::query_as::<_, ReportedStrategy>(
        "SELECT code, name, portfolio_id, owner_principal_id, state, stale_after_secs, created_at, archived_at \
           FROM strategy WHERE mode = 'REPORTED' AND ($2 OR owner_principal_id = $1) ORDER BY code",
    )
    .bind(auth.principal_id)
    .bind(admin)
    .fetch_all(state.pool())
    .await
    .map_err(db)?;
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use crate::strategies::test_support as ts;

    fn ctx(id: Uuid) -> Extension<AuthContext> {
        Extension(AuthContext::machine(id, "t".into()))
    }

    async fn human(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(id)
            .bind(format!("h-{id}"))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn setup() -> (PgPool, AppState, Uuid) {
        let pool = migrated_db().await;
        let state = ts::state(pool.clone());
        let me = human(&pool).await;
        (pool, state, me)
    }

    fn req(code: &str) -> Json<CreateReported> {
        Json(CreateReported { code: code.into(), name: format!("{code} name"), base_currency: Some("USD".into()) })
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn trader_creates_reported_strategy_with_private_portfolio() {
        let (pool, state, me) = setup().await;
        let other = human(&pool).await;
        let Json(c) = create(State(state), ctx(me), req("alpha")).await.unwrap();
        let pcode: String = sqlx::query_scalar("SELECT code FROM portfolio WHERE id = $1")
            .bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert_eq!(pcode, "RPT-alpha");
        let access: String = sqlx::query_scalar(
            "SELECT access FROM user_group_portfolio WHERE group_id = $1 AND portfolio_id = $2",
        )
        .bind(c.group_id).bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert_eq!(access, "manage");
        let visible: bool = sqlx::query_scalar(
            "SELECT can_view FROM portfolio_access WHERE principal_id = $1 AND portfolio_id = $2",
        )
        .bind(me).bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert!(visible);
        let theirs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM portfolio_access WHERE principal_id = $1 AND portfolio_id = $2",
        )
        .bind(other).bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert_eq!(theirs, 0);
        let (gname, gcode): (String, Option<String>) =
            sqlx::query_as("SELECT name, strategy_code FROM user_group WHERE id = $1")
                .bind(c.group_id).fetch_one(&pool).await.unwrap();
        assert_eq!((gname.as_str(), gcode.as_deref()), ("strategy alpha", Some("alpha")));
        let (mode, owner): (String, Uuid) =
            sqlx::query_as("SELECT mode, owner_principal_id FROM strategy WHERE code = 'alpha'")
                .fetch_one(&pool).await.unwrap();
        assert_eq!((mode.as_str(), owner), ("REPORTED", me));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn token_is_shown_once_and_authenticates() {
        let (pool, state, me) = setup().await;
        let Json(c) = create(State(state.clone()), ctx(me), req("beta")).await.unwrap();
        assert_eq!(token::authenticate(&pool, &c.token).await.unwrap(), "beta");
        // the owner's group carries the manage-level grants
        crate::authz::require(&pool, me, crate::authz::Permission::LimitsManage, c.portfolio_id).await.unwrap();
        crate::authz::require(&pool, me, crate::authz::Permission::OrdersCancelAny, c.portfolio_id).await.unwrap();
        assert!(token::authenticate(&pool, "nonsense").await.is_err());
        let Json(mine) = list_mine(State(state), ctx(me)).await.unwrap();
        assert_eq!(mine.len(), 1);
        let json = serde_json::to_value(&mine).unwrap();
        assert!(json[0].get("token").is_none(), "{json}");
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM strategy_reporting_token")
            .fetch_one(&pool).await.unwrap();
        assert!(!c.token.contains(&stored));
        assert_eq!(stored.len(), 64);
        let (id, _) = c.token.split_once('.').unwrap();
        assert!(token::authenticate(&pool, &format!("{id}.sk_wrong")).await.is_err());
        let unknown = format!("{}.sk_x", Uuid::new_v4().simple());
        assert!(token::authenticate(&pool, &unknown).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn rotate_keeps_old_token_24h() {
        let (pool, state, me) = setup().await;
        let Json(c) = create(State(state.clone()), ctx(me), req("gamma")).await.unwrap();
        let Json(new) = rotate_token(State(state.clone()), ctx(me), Path("gamma".into())).await.unwrap();
        assert_eq!(token::authenticate(&pool, &c.token).await.unwrap(), "gamma");
        assert_eq!(token::authenticate(&pool, &new.token).await.unwrap(), "gamma");
        sqlx::query("UPDATE strategy_reporting_token SET expires_at = now() - interval '1 second' WHERE expires_at IS NOT NULL")
            .execute(&pool).await.unwrap();
        assert!(token::authenticate(&pool, &c.token).await.is_err());
        assert!(token::authenticate(&pool, &new.token).await.is_ok());
        // Rotating again revokes the graced token: at most one in grace.
        let Json(third) = rotate_token(State(state), ctx(me), Path("gamma".into())).await.unwrap();
        let graced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM strategy_reporting_token WHERE revoked_at IS NULL AND expires_at IS NOT NULL",
        )
        .fetch_one(&pool).await.unwrap();
        assert_eq!(graced, 1);
        assert!(token::authenticate(&pool, &new.token).await.is_ok());
        assert!(token::authenticate(&pool, &third.token).await.is_ok());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn revoke_stops_auth() {
        let (pool, state, me) = setup().await;
        let Json(c) = create(State(state.clone()), ctx(me), req("delta")).await.unwrap();
        let s = revoke_token(State(state), ctx(me), Path("delta".into())).await.unwrap();
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(token::authenticate(&pool, &c.token).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn archive_revokes_tokens_blocks_rotate_and_keeps_history() {
        let (pool, state, me) = setup().await;
        let other = human(&pool).await;
        let Json(c) = create(State(state.clone()), ctx(me), req("arch")).await.unwrap();
        let err = archive(State(state.clone()), ctx(other), Path("arch".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(archive(State(state.clone()), ctx(me), Path("arch".into())).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(token::authenticate(&pool, &c.token).await.is_err());
        let err = rotate_token(State(state.clone()), ctx(me), Path("arch".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        // Idempotent; strategy and portfolio stay.
        assert_eq!(archive(State(state.clone()), ctx(me), Path("arch".into())).await.unwrap(), StatusCode::NO_CONTENT);
        let Json(mine) = list_mine(State(state), ctx(me)).await.unwrap();
        assert!(mine[0].archived_at.is_some());
        let portfolio: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio WHERE id = $1")
            .bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert_eq!(portfolio, 1);
        let pstatus: String = sqlx::query_scalar("SELECT status FROM portfolio WHERE id = $1")
            .bind(c.portfolio_id).fetch_one(&pool).await.unwrap();
        assert_eq!(pstatus, "CLOSED");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn non_owner_cannot_rotate() {
        let (pool, state, me) = setup().await;
        let other = human(&pool).await;
        let _ = create(State(state.clone()), ctx(me), req("eps")).await.unwrap();
        let err = rotate_token(State(state.clone()), ctx(other), Path("eps".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let err = revoke_token(State(state.clone()), ctx(other), Path("eps".into())).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let Json(theirs) = list_mine(State(state), ctx(other)).await.unwrap();
        assert!(theirs.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn duplicate_code_is_conflict() {
        let (pool, state, me) = setup().await;
        let _ = create(State(state.clone()), ctx(me), req("zeta")).await.unwrap();
        let err = create(State(state), ctx(me), req("zeta")).await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        for (table, n) in [("portfolio WHERE code LIKE 'RPT-%'", 1i64), ("strategy", 1), ("user_group WHERE strategy_code IS NOT NULL", 1), ("strategy_reporting_token", 1)] {
            let got: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&pool).await.unwrap();
            assert_eq!(got, n, "{table}");
        }
    }

    /// The FIX/REST registry and the admin strategy list skip REPORTED
    /// rows (no principal or comp ids); the admin delete refuses them.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn reported_rows_do_not_break_strategy_readers() {
        let (pool, state, me) = setup().await;
        let _ = create(State(state.clone()), ctx(me), req("eta")).await.unwrap();
        let via = ts::seed_strategy(&pool).await;
        let all = crate::strategies::load_all(&pool).await.unwrap();
        assert_eq!(all.iter().map(|s| s.code.as_str()).collect::<Vec<_>>(), vec![via.as_str()]);
        state.strategies().reload(&pool).await.unwrap();
        let rows = crate::strategies::admin::list_rows(&state, None).await.unwrap();
        assert_eq!(rows.iter().map(|s| s.code.as_str()).collect::<Vec<_>>(), vec![via.as_str()]);
        let err = crate::strategies::admin::delete_strategy(State(state), Path("eta".into())).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }
}
