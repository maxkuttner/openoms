//! Role and portfolio permission checks. A role is data (`role_permission`);
//! a portfolio grant says where a principal may act. Both must allow.

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

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

    /// Order actions need the portfolio grant's `can_trade`, not just `can_view`.
    fn needs_trade(self) -> bool {
        matches!(
            self,
            Permission::OrdersCreate | Permission::OrdersCancelOwn | Permission::OrdersCancelAny
        )
    }
}

/// Portfolios on which `principal_id` holds `perm`: ACTIVE, role has the
/// permission, grant has `can_view` (and `can_trade` for order actions).
const VISIBLE_SQL: &str = "\
    SELECT g.portfolio_id \
      FROM principal p \
      JOIN principal_role pr ON pr.principal_id = p.id \
      JOIN role_permission rp ON rp.role_code = pr.role_code AND rp.permission = $2 \
      JOIN principal_portfolio_grant g ON g.principal_id = p.id \
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

pub async fn allowed(
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

/// 403 unless `allowed`; a database failure is a 500.
pub async fn require(
    pool: &PgPool,
    principal_id: Uuid,
    perm: Permission,
    portfolio_id: Uuid,
) -> Result<(), ApiError> {
    match allowed(pool, principal_id, perm, portfolio_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "not authorized".to_string(),
        }),
        Err(err) => Err(crate::handlers::internal("permission check failed", err)),
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
pub fn require_mfa(ctx: &crate::auth::AuthContext) -> Result<(), ApiError> {
    use crate::auth::AuthMethod;
    match ctx.auth_method {
        AuthMethod::ApiKey | AuthMethod::HostCli => Ok(()),
        AuthMethod::Local | AuthMethod::Oidc if ctx.mfa => Ok(()),
        _ => Err(ApiError { status: StatusCode::FORBIDDEN, message: MFA_REQUIRED_BODY.to_string() }),
    }
}
