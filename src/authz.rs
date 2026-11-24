//! Role and portfolio permission checks. A role is data (`role_permission`);
//! `portfolio_access` (a view) says where a principal may act and with which
//! role: a person's highest group access there (manage = pm, trade = trader,
//! view = viewer), or a strategy's direct grant with its own role. Both must
//! allow; no row means no access.

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use crate::access_grants::GrantKey;
use crate::auth::{AuthContext, AuthMethod};
use crate::handlers::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    OrdersViewOwn,
    OrdersViewAll,
    OrdersCreate,
    OrdersCancelOwn,
    OrdersCancelAny,
    PositionsView,
    LimitsView,
    LimitsManage,
    StrategiesManage,
}

impl Permission {
    /// The string stored in `role_permission.permission`.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::OrdersViewOwn => "orders.view_own",
            Permission::OrdersViewAll => "orders.view_all",
            Permission::OrdersCreate => "orders.create",
            Permission::OrdersCancelOwn => "orders.cancel_own",
            Permission::OrdersCancelAny => "orders.cancel_any",
            Permission::PositionsView => "positions.view",
            Permission::LimitsView => "limits.view",
            Permission::LimitsManage => "limits.manage",
            Permission::StrategiesManage => "strategies.manage",
        }
    }

    /// The access grant this permission also needs (on top of the portfolio
    /// role), if any.
    pub fn grant(self) -> Option<GrantKey> {
        match self {
            Permission::OrdersCreate | Permission::OrdersCancelOwn => Some(GrantKey::Orders),
            Permission::OrdersCancelAny => Some(GrantKey::CancelAny),
            Permission::LimitsView => Some(GrantKey::LimitsView),
            Permission::LimitsManage => Some(GrantKey::LimitsSet),
            _ => None,
        }
    }

    /// Order actions need the portfolio grant's `can_trade`, not just `can_view`.
    fn needs_trade(self) -> bool {
        matches!(
            self,
            Permission::OrdersCreate | Permission::OrdersCancelOwn | Permission::OrdersCancelAny
        )
    }
}

/// Portfolios on which `principal_id` holds `perm`: ACTIVE, the access row's
/// role has the permission, the row has `can_view` (and `can_trade` for order
/// actions).
const VISIBLE_SQL: &str = "\
    SELECT g.portfolio_id \
      FROM principal p \
      JOIN portfolio_access g ON g.principal_id = p.id \
      JOIN role_permission rp ON rp.role_code = g.role_code AND rp.permission = $2 \
     WHERE p.id = $1 AND p.status = 'ACTIVE' \
       AND g.can_view AND (g.can_trade OR NOT $3)";

pub async fn visible_portfolios(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(VISIBLE_SQL)
        .bind(principal_id)
        .bind(perm.as_str())
        .bind(perm.needs_trade())
        .fetch_all(pool)
        .await
}

/// Does `principal_id` hold `key`? Only people are grant-checked: a strategy
/// (any non-HUMAN principal) passes; an unknown id is denied. The host token
/// (nil id) is exempted by `require_grant`, and `check` never reaches here
/// without a portfolio access row.
/// Active admins hold every key. One query.
pub async fn has_grant(pool: &PgPool, principal_id: Uuid, key: GrantKey) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal WHERE id = $1 AND principal_type <> 'HUMAN') \
             OR EXISTS (SELECT 1 FROM principal_admin a JOIN principal p ON p.id = a.principal_id \
                         WHERE a.principal_id = $1 AND p.status = 'ACTIVE') \
             OR EXISTS (SELECT 1 FROM access_grant WHERE subject_type = 'default' AND grant_key = $2) \
             OR EXISTS (SELECT 1 FROM access_grant a JOIN user_group_member m ON m.group_id = a.subject_id \
                         WHERE a.subject_type = 'group' AND m.principal_id = $1 AND a.grant_key = $2)",
    )
    .bind(principal_id)
    .bind(key.as_str())
    .fetch_one(pool)
    .await
}

/// Why a permission check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// No portfolio access (or the role lacks the permission).
    Access,
    /// Portfolio access is fine, but the person lacks the access grant.
    Grant(GrantKey),
}

/// 403 `not granted: <key>`.
pub fn not_granted(key: GrantKey) -> ApiError {
    ApiError { status: StatusCode::FORBIDDEN, message: format!("not granted: {}", key.as_str()) }
}

impl Denied {
    /// The 403 for this denial; `access` is the message for a missing
    /// portfolio permission.
    pub fn into_api(self, access: &str) -> ApiError {
        match self {
            Denied::Access => ApiError { status: StatusCode::FORBIDDEN, message: access.to_string() },
            Denied::Grant(key) => not_granted(key),
        }
    }
}

async fn portfolio_allowed(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
    portfolio_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS ({VISIBLE_SQL} AND g.portfolio_id = $4)"
    ))
    .bind(principal_id)
    .bind(perm.as_str())
    .bind(perm.needs_trade())
    .bind(portfolio_id)
    .fetch_one(pool)
    .await
}

/// The portfolio check, then the permission's access grant (if any).
pub async fn check(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
    portfolio_id: Uuid,
) -> Result<Result<(), Denied>, sqlx::Error> {
    if !portfolio_allowed(pool, principal_id, perm, portfolio_id).await? {
        return Ok(Err(Denied::Access));
    }
    match perm.grant() {
        Some(key) if !has_grant(pool, principal_id, key).await? => Ok(Err(Denied::Grant(key))),
        _ => Ok(Ok(())),
    }
}

/// Portfolio access allows `perm` AND the principal holds its grant (if any).
pub async fn allowed(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
    portfolio_id: Uuid,
) -> Result<bool, sqlx::Error> {
    Ok(check(pool, principal_id, perm, portfolio_id).await?.is_ok())
}

/// 403 unless `allowed` (`not authorized`, or `not granted: <key>`); a
/// database failure is a 500.
pub async fn require(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
    portfolio_id: Uuid,
) -> Result<(), ApiError> {
    match check(pool, principal_id, perm, portfolio_id).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(denied)) => Err(denied.into_api("not authorized")),
        Err(err) => Err(crate::handlers::internal("permission check failed", err)),
    }
}

/// 403 `not granted: <key>` unless the caller holds `key`. The host token
/// skips the check; strategies pass inside `has_grant`.
pub async fn require_grant(pool: &PgPool, ctx: &AuthContext, key: GrantKey) -> Result<(), ApiError> {
    if ctx.auth_method == AuthMethod::HostCli {
        return Ok(());
    }
    match has_grant(pool, ctx.principal_id, key).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(not_granted(key)),
        Err(err) => Err(crate::handlers::internal("grant check failed", err)),
    }
}

/// The 403 body for an action that needs a session which passed MFA.
pub const MFA_REQUIRED_BODY: &str = r#"{"error":"mfa_required"}"#;

/// MFA gate for order actions (submit, cancel, allocate), strategy create and
/// handover, limit changes, and every admin mutation. Reads never call it.
///
/// - Machine credentials (API keys, strategies, the host token) are exempt.
/// - A local-password session must have passed TOTP.
/// - An OIDC session passes when its login met `[auth.oidc].require_amr` —
///   evaluated once at login and stored as `mfa_verified_at`. An empty
///   `require_amr` trusts the identity provider's own MFA policy.
/// - Anything else (the legacy shared password) is refused.
pub fn require_mfa(ctx: &AuthContext) -> Result<(), ApiError> {
    match ctx.auth_method {
        AuthMethod::ApiKey | AuthMethod::HostCli => Ok(()),
        AuthMethod::Local | AuthMethod::Oidc if ctx.mfa => Ok(()),
        _ => Err(ApiError::mfa_required()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::fresh_db;
    use crate::setup::database::migrate;
    use sqlx::postgres::PgConnectOptions;

    /// Migrated disposable database whose connections resolve unqualified
    /// table names in `oms`, as the runtime role does.
    async fn db() -> PgPool {
        let admin = fresh_db().await;
        migrate::ensure_tracking(&admin).await.unwrap();
        migrate::apply_all(&admin).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&admin)
            .await
            .unwrap();
        sqlx::query(&format!(
            "ALTER DATABASE {name} SET search_path = oms, public"
        ))
        .execute(&admin)
        .await
        .unwrap();
        let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL").unwrap();
        let options: PgConnectOptions = url.parse().unwrap();
        PgPool::connect_with(options.database(&name)).await.unwrap()
    }

    async fn user(pool: &PgPool, role: &str, status: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', $3)",
        )
        .bind(id)
        .bind(format!("u-{}", id.simple()))
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
            .bind(id)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn portfolio(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')",
        )
        .bind(id)
        .bind(format!("p-{}", id.simple()))
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn grant(
        pool: &PgPool,
        principal: Uuid,
        portfolio: Uuid,
        can_trade: bool,
        can_view: bool,
    ) {
        sqlx::query(
            "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(principal)
        .bind(portfolio)
        .bind(can_trade)
        .bind(can_view)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn permission_strings_match_seeded_roles() {
        assert_eq!(Permission::OrdersViewOwn.as_str(), "orders.view_own");
        assert_eq!(Permission::OrdersViewAll.as_str(), "orders.view_all");
        assert_eq!(Permission::OrdersCreate.as_str(), "orders.create");
        assert_eq!(Permission::OrdersCancelOwn.as_str(), "orders.cancel_own");
        assert_eq!(Permission::OrdersCancelAny.as_str(), "orders.cancel_any");
        assert_eq!(Permission::PositionsView.as_str(), "positions.view");
        assert_eq!(Permission::LimitsView.as_str(), "limits.view");
        assert_eq!(Permission::LimitsManage.as_str(), "limits.manage");
        assert_eq!(Permission::StrategiesManage.as_str(), "strategies.manage");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn viewer_cannot_create() {
        let pool = db().await;
        let (u, p) = (
            user(&pool, "viewer", "ACTIVE").await,
            portfolio(&pool).await,
        );
        grant(&pool, u, p, true, true).await;
        assert!(!allowed(&pool, u, Permission::OrdersCreate, p)
            .await
            .unwrap());
        assert!(allowed(&pool, u, Permission::PositionsView, p)
            .await
            .unwrap());
        assert_eq!(
            require(&pool, u, Permission::OrdersCreate, p)
                .await
                .unwrap_err()
                .status,
            axum::http::StatusCode::FORBIDDEN
        );
        require(&pool, u, Permission::PositionsView, p)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn trader_can_create_with_can_trade_only() {
        let pool = db().await;
        let u = user(&pool, "trader", "ACTIVE").await;
        let (traded, view_only) = (portfolio(&pool).await, portfolio(&pool).await);
        grant(&pool, u, traded, true, true).await;
        grant(&pool, u, view_only, false, true).await;
        assert!(allowed(&pool, u, Permission::OrdersCreate, traded)
            .await
            .unwrap());
        assert!(allowed(&pool, u, Permission::OrdersCancelOwn, traded)
            .await
            .unwrap());
        assert!(!allowed(&pool, u, Permission::OrdersCreate, view_only)
            .await
            .unwrap());
        assert!(!allowed(&pool, u, Permission::OrdersCancelOwn, view_only)
            .await
            .unwrap());
        assert!(allowed(&pool, u, Permission::OrdersViewOwn, view_only)
            .await
            .unwrap());
        // A trader's role has no limits.manage whatever the grant says.
        assert!(!allowed(&pool, u, Permission::LimitsManage, traded)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn pm_can_manage_limits() {
        let pool = db().await;
        let (u, p) = (user(&pool, "pm", "ACTIVE").await, portfolio(&pool).await);
        grant(&pool, u, p, false, true).await;
        // limits_set is not a default grant: an admin grants it.
        crate::access_grants::grant_via_group(&pool, u, &[crate::access_grants::GrantKey::LimitsSet]).await;
        assert!(allowed(&pool, u, Permission::LimitsManage, p)
            .await
            .unwrap());
        // Needs can_trade for order actions even as PM.
        assert!(!allowed(&pool, u, Permission::OrdersCancelAny, p)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn disabled_principal_is_never_allowed() {
        let pool = db().await;
        let (u, p) = (user(&pool, "pm", "DISABLED").await, portfolio(&pool).await);
        grant(&pool, u, p, true, true).await;
        assert!(!allowed(&pool, u, Permission::OrdersViewOwn, p)
            .await
            .unwrap());
        assert!(visible_portfolios(&pool, u, Permission::OrdersViewOwn)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invited_principal_is_never_allowed() {
        let pool = db().await;
        let (u, p) = (user(&pool, "pm", "INVITED").await, portfolio(&pool).await);
        grant(&pool, u, p, true, true).await;
        assert!(!allowed(&pool, u, Permission::OrdersViewOwn, p)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn no_grant_means_no_access() {
        let pool = db().await;
        let (u, p) = (user(&pool, "pm", "ACTIVE").await, portfolio(&pool).await);
        assert!(!allowed(&pool, u, Permission::OrdersViewOwn, p)
            .await
            .unwrap());
        assert!(visible_portfolios(&pool, u, Permission::OrdersViewOwn)
            .await
            .unwrap()
            .is_empty());
        // A grant with can_view = false does not count either.
        grant(&pool, u, p, true, false).await;
        assert!(!allowed(&pool, u, Permission::OrdersViewOwn, p)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn visible_portfolios_lists_granted_portfolios_with_permission() {
        let pool = db().await;
        let u = user(&pool, "trader", "ACTIVE").await;
        let (a, b, c) = (
            portfolio(&pool).await,
            portfolio(&pool).await,
            portfolio(&pool).await,
        );
        grant(&pool, u, a, true, true).await;
        grant(&pool, u, b, false, true).await;
        grant(&pool, u, c, true, false).await;
        let mut seen = visible_portfolios(&pool, u, Permission::OrdersViewOwn)
            .await
            .unwrap();
        seen.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(seen, want);
        // Trade permissions narrow it to can_trade grants.
        assert_eq!(
            visible_portfolios(&pool, u, Permission::OrdersCreate)
                .await
                .unwrap(),
            vec![a]
        );
        assert!(visible_portfolios(&pool, u, Permission::LimitsManage)
            .await
            .unwrap()
            .is_empty());
    }
}

/// Access-grant enforcement on top of portfolio access.
#[cfg(test)]
mod grant_tests {
    use super::*;
    use crate::access_grants::{set_default_grants, set_group_grants};
    use crate::authz_schema_tests::migrated_db;
    use crate::groups_api::tests::{admin, group, person, portfolio, state};
    use crate::groups_api::Access;
    use axum::extract::{Extension, State};
    use axum::Json;

    async fn defaults(pool: &PgPool, keys: &[GrantKey]) {
        let mut tx = pool.begin().await.unwrap();
        set_default_grants(&mut tx, &admin(), keys).await.unwrap();
        tx.commit().await.unwrap();
    }

    async fn group_keys(pool: &PgPool, g: Uuid, keys: &[GrantKey]) {
        let mut tx = pool.begin().await.unwrap();
        set_group_grants(&mut tx, &admin(), g, keys).await.unwrap();
        tx.commit().await.unwrap();
    }

    fn signed_in(id: Uuid) -> AuthContext {
        AuthContext { principal_id: id, principal_code: "me".into(), auth_method: AuthMethod::Local, mfa: true }
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cancel_any_needs_grant() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let g = group(&st, "pms", &[me], &[(pf, Access::Manage)]).await;
        let e = require(&pool, me, Permission::OrdersCancelAny, pf).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::FORBIDDEN, "not granted: cancel_any"));
        assert!(allowed(&pool, me, Permission::OrdersCancelOwn, pf).await.unwrap(), "own cancels ride on orders");
        group_keys(&pool, g, &[GrantKey::CancelAny]).await;
        assert!(allowed(&pool, me, Permission::OrdersCancelAny, pf).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn limits_set_needs_grant() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let g = group(&st, "pms", &[me], &[(pf, Access::Manage)]).await;
        let e = require(&pool, me, Permission::LimitsManage, pf).await.unwrap_err();
        assert_eq!(e.message, "not granted: limits_set");
        let e = crate::limits_api::put_limit(
            State(st.clone()),
            Extension(signed_in(me)),
            axum::extract::Path((pf, "1".to_string())),
            Json(serde_json::from_value(serde_json::json!({})).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::FORBIDDEN, "not granted: limits_set"));
        group_keys(&pool, g, &[GrantKey::LimitsSet]).await;
        require(&pool, me, Permission::LimitsManage, pf).await.unwrap();
        defaults(&pool, &[]).await;
        assert_eq!(require(&pool, me, Permission::LimitsView, pf).await.unwrap_err().message, "not granted: limits_view");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_skips_grants_not_access() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf, other) = (person(&pool).await, portfolio(&pool).await, portfolio(&pool).await);
        sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, 'test')")
            .bind(me).execute(&pool).await.unwrap();
        group(&st, "pms", &[me], &[(pf, Access::Manage)]).await;
        defaults(&pool, &[]).await;
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap());
        assert!(allowed(&pool, me, Permission::OrdersCancelAny, pf).await.unwrap());
        assert!(allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap());
        require_grant(&pool, &signed_in(me), GrantKey::ApiKeys).await.unwrap();
        let e = require(&pool, me, Permission::OrdersCreate, other).await.unwrap_err();
        assert_eq!(e.message, "not authorized", "admin still needs portfolio access");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn strategy_token_not_grant_checked() {
        let pool = migrated_db().await;
        let pf = portfolio(&pool).await;
        let s = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')")
            .bind(s).bind(format!("s-{}", s.simple())).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'strategy')")
            .bind(s).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view, can_trade) VALUES ($1, $2, $3, true, true)")
            .bind(Uuid::new_v4()).bind(s).bind(pf).execute(&pool).await.unwrap();
        defaults(&pool, &[]).await;
        assert!(allowed(&pool, s, Permission::OrdersCreate, pf).await.unwrap());
        require_grant(&pool, &AuthContext::machine(s, "s".into()), GrantKey::Options).await.unwrap();
        require_grant(&pool, &crate::admin_bootstrap::host_context(), GrantKey::ApiKeys).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn unknown_principal_is_denied_grants() {
        let pool = migrated_db().await;
        defaults(&pool, &[]).await;
        assert!(!has_grant(&pool, Uuid::new_v4(), GrantKey::Orders).await.unwrap());
        let e = require_grant(&pool, &AuthContext::machine(Uuid::new_v4(), "ghost".into()), GrantKey::ApiKeys).await.unwrap_err();
        assert_eq!(e.message, "not granted: api_keys");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn strategies_grant_blocks_create() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let me = person(&pool).await;
        defaults(&pool, &[GrantKey::Orders]).await;
        let body = || Json(crate::reported::strategy::CreateReported { code: "rs".into(), name: "rs".into(), base_currency: None });
        let e = crate::reported::strategy::create(State(st.clone()), Extension(signed_in(me)), body()).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::FORBIDDEN, "not granted: strategies"));
        defaults(&pool, &[GrantKey::Strategies]).await;
        crate::reported::strategy::create(State(st.clone()), Extension(signed_in(me)), body()).await.unwrap();
        let path = || axum::extract::Path("rs".to_string());
        let e = crate::reported::strategy::rotate_token(State(st.clone()), Extension(signed_in(me)), path()).await.unwrap_err();
        assert_eq!(e.message, "not granted: strategy_tokens");
        let e = crate::reported::strategy::revoke_token(State(st.clone()), Extension(signed_in(me)), path()).await.unwrap_err();
        assert_eq!(e.message, "not granted: strategy_tokens");
        let e = crate::reported::strategy::archive(State(st.clone()), Extension(signed_in(me)), path()).await.unwrap_err();
        assert_eq!(e.message, "not granted: strategy_tokens");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn api_keys_grant_blocks_key_create() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let me = person(&pool).await;
        defaults(&pool, &[GrantKey::Orders]).await;
        let body = || Json(crate::admin::CreateKey { name: Some("k".into()) });
        let e = crate::users_api::create_own_key(State(st.clone()), Extension(signed_in(me)), body()).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::FORBIDDEN, "not granted: api_keys"));
        defaults(&pool, &[GrantKey::ApiKeys]).await;
        crate::users_api::create_own_key(State(st.clone()), Extension(signed_in(me)), body()).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn me_lists_grants() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let me = person(&pool).await;
        defaults(&pool, &[GrantKey::Orders, GrantKey::ApiKeys]).await;
        let g = group(&st, "pms", &[me], &[]).await;
        group_keys(&pool, g, &[GrantKey::CancelAny]).await;
        let Json(out) = crate::auth_api::me(State(st.clone()), Extension(signed_in(me))).await.unwrap();
        assert_eq!(out.grants, vec!["orders", "cancel_any", "api_keys"]);
    }
}
