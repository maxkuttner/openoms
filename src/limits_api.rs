//! PM-managed risk limits in the trader API: the same `risk_limits` rows and
//! validation as `/admin/risk-limits`, scoped to the caller's portfolios.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use uuid::Uuid;

use crate::admin::{
    republish_colo_policy, validate_trading_state, RiskLimit, UpdateRiskLimit, RISK_LIMIT_SELECT,
};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::authz::{self, Permission};
use crate::handlers::{internal, ApiError};

/// Every limit row of one portfolio.
pub async fn list_limits(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(portfolio_id): Path<Uuid>,
) -> Result<Json<Vec<RiskLimit>>, ApiError> {
    authz::require(state.pool(), auth.principal_id, Permission::LimitsView, portfolio_id).await?;
    let sql = format!(
        "SELECT {RISK_LIMIT_SELECT} FROM risk_limits WHERE portfolio_id = $1 ORDER BY instrument_id"
    );
    let rows = sqlx::query_as::<_, RiskLimit>(&sql)
        .bind(portfolio_id)
        .fetch_all(state.pool())
        .await
        .map_err(|e| internal("failed to load limits", e))?;
    Ok(Json(rows))
}

/// Create or update the limit for one instrument. Body and semantics follow the
/// admin PATCH: omitted fields keep their value (a new row starts ACTIVE).
pub async fn put_limit(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path((portfolio_id, instrument_id)): Path<(Uuid, String)>,
    Json(payload): Json<UpdateRiskLimit>,
) -> Result<Json<RiskLimit>, ApiError> {
    authz::require(state.pool(), auth.principal_id, Permission::LimitsManage, portfolio_id).await?;
    if let Some(ts) = payload.trading_state.as_deref() {
        validate_trading_state(ts)?;
    }
    let ctx = "failed to save limit";
    let mut tx = state.pool().begin().await.map_err(|e| internal(ctx, e))?;
    let before = sqlx::query_as::<_, RiskLimit>(&format!(
        "SELECT {RISK_LIMIT_SELECT} FROM risk_limits WHERE portfolio_id = $1 AND instrument_id = $2 FOR UPDATE"
    ))
    .bind(portfolio_id)
    .bind(&instrument_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| internal(ctx, e))?;
    let after = sqlx::query_as::<_, RiskLimit>(&format!(
        "INSERT INTO risk_limits \
           (portfolio_id, instrument_id, trading_state, max_order_quantity, \
            max_order_notional, max_position_quantity, max_position_notional) \
         VALUES ($1, $2, COALESCE($3, 'ACTIVE'), $4, $5, $6, $7) \
         ON CONFLICT (portfolio_id, instrument_id) DO UPDATE SET \
            trading_state         = COALESCE($3, risk_limits.trading_state), \
            max_order_quantity    = COALESCE($4, risk_limits.max_order_quantity), \
            max_order_notional    = COALESCE($5, risk_limits.max_order_notional), \
            max_position_quantity = COALESCE($6, risk_limits.max_position_quantity), \
            max_position_notional = COALESCE($7, risk_limits.max_position_notional), \
            updated_at = now() \
         RETURNING {RISK_LIMIT_SELECT}"
    ))
    .bind(portfolio_id)
    .bind(&instrument_id)
    .bind(payload.trading_state)
    .bind(payload.max_order_quantity)
    .bind(payload.max_order_notional)
    .bind(payload.max_position_quantity)
    .bind(payload.max_position_notional)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        // CHECK constraint: a non-positive maximum.
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23514") => ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "limits must be greater than zero".to_string(),
        },
        _ => internal(ctx, e),
    })?;
    let detail = serde_json::json!({
        "portfolio_id": portfolio_id,
        "instrument_id": instrument_id,
        "before": before,
        "after": after,
    });
    sqlx::query("INSERT INTO audit_event (actor, action, target_principal_id, detail) VALUES ($1, 'limits.changed', NULL, $2)")
        .bind(&auth.principal_code)
        .bind(detail)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal(ctx, e))?;
    tx.commit().await.map_err(|e| internal(ctx, e))?;
    republish_colo_policy(&state, portfolio_id).await;
    Ok(Json(after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::tests::{test_app_state, test_pool};
    use sqlx::PgPool;

    async fn user(pool: &PgPool, role: &str) -> AuthContext {
        let id = Uuid::new_v4();
        let code = format!("limits-{role}-{id}");
        sqlx::query("INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, 'HUMAN', $2, 'ACTIVE')")
            .bind(id).bind(&code).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
            .bind(id).bind(role).execute(pool).await.unwrap();
        AuthContext { principal_id: id, principal_code: code }
    }

    async fn portfolio(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, $2, 'ACTIVE')")
            .bind(id).bind(format!("limits-{id}")).execute(pool).await.unwrap();
        id
    }

    async fn grant(pool: &PgPool, who: &AuthContext, portfolio: Uuid) {
        sqlx::query("INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) VALUES ($1, $2, $3, true, true)")
            .bind(Uuid::new_v4()).bind(who.principal_id).bind(portfolio).execute(pool).await.unwrap();
    }

    fn body(max_order_quantity: f64) -> UpdateRiskLimit {
        UpdateRiskLimit {
            trading_state: None,
            max_order_quantity: Some(max_order_quantity),
            max_order_notional: None,
            max_position_quantity: None,
            max_position_notional: None,
        }
    }

    async fn put(pool: &PgPool, who: &AuthContext, p: Uuid, i: &str, b: UpdateRiskLimit) -> Result<Json<RiskLimit>, ApiError> {
        put_limit(State(test_app_state(pool.clone())), Extension(who.clone()), Path((p, i.to_string())), Json(b)).await
    }

    async fn list(pool: &PgPool, who: &AuthContext, p: Uuid) -> Result<Json<Vec<RiskLimit>>, ApiError> {
        list_limits(State(test_app_state(pool.clone())), Extension(who.clone()), Path(p)).await
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn trader_can_view_limits() {
        let pool = test_pool().await;
        let (pm, trader) = (user(&pool, "pm").await, user(&pool, "trader").await);
        let p = portfolio(&pool).await;
        grant(&pool, &pm, p).await;
        grant(&pool, &trader, p).await;
        let _ = put(&pool, &pm, p, "BTC", body(5.0)).await.unwrap();
        let rows = list(&pool, &trader, p).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].instrument_id, "BTC");
        assert_eq!(rows[0].max_order_quantity, Some(5.0));
        assert_eq!(rows[0].trading_state, "ACTIVE");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn trader_cannot_change_limits() {
        let pool = test_pool().await;
        let trader = user(&pool, "trader").await;
        let p = portfolio(&pool).await;
        grant(&pool, &trader, p).await;
        let err = put(&pool, &trader, p, "BTC", body(5.0)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM risk_limits WHERE portfolio_id = $1")
            .bind(p).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn pm_change_is_audited_with_before_after() {
        let pool = test_pool().await;
        let pm = user(&pool, "pm").await;
        let p = portfolio(&pool).await;
        grant(&pool, &pm, p).await;
        let _ = put(&pool, &pm, p, "ETH", body(5.0)).await.unwrap();
        let after = put(&pool, &pm, p, "ETH", body(9.0)).await.unwrap().0;
        assert_eq!(after.max_order_quantity, Some(9.0));
        let details: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT detail FROM audit_event WHERE action = 'limits.changed' AND actor = $1 \
             AND target_principal_id IS NULL ORDER BY id",
        )
        .bind(&pm.principal_code).fetch_all(&pool).await.unwrap();
        assert_eq!(details.len(), 2);
        assert!(details[0]["before"].is_null());
        assert_eq!(details[1]["before"]["max_order_quantity"], 5.0);
        assert_eq!(details[1]["after"]["max_order_quantity"], 9.0);
        assert_eq!(details[1]["instrument_id"], "ETH");
        assert_eq!(details[1]["portfolio_id"], p.to_string());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn idor_limits_outside_scope_is_403() {
        let pool = test_pool().await;
        let pm = user(&pool, "pm").await;
        let (mine, other) = (portfolio(&pool).await, portfolio(&pool).await);
        grant(&pool, &pm, mine).await;
        assert_eq!(list(&pool, &pm, other).await.unwrap_err().status, StatusCode::FORBIDDEN);
        assert_eq!(put(&pool, &pm, other, "BTC", body(1.0)).await.unwrap_err().status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn invalid_bodies_are_400() {
        let pool = test_pool().await;
        let pm = user(&pool, "pm").await;
        let p = portfolio(&pool).await;
        grant(&pool, &pm, p).await;
        let mut b = body(1.0);
        b.trading_state = Some("BOGUS".into());
        assert_eq!(put(&pool, &pm, p, "BTC", b).await.unwrap_err().status, StatusCode::BAD_REQUEST);
        assert_eq!(put(&pool, &pm, p, "BTC", body(-1.0)).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    }
}
