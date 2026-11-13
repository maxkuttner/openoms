//! Trader-app strategy routes: a PM manages the strategies on portfolios where
//! they hold `strategies.manage`.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use uuid::Uuid;

use super::admin::{self, CreateStrategy, Handover, StrategyRow, PICK_RESPONSIBLE};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::authz::{self, Permission};
use crate::handlers::{internal, ApiError};

fn db(e: sqlx::Error) -> ApiError {
    internal("database error", e)
}

/// A PM may make themselves or a user granted on the portfolio responsible.
/// That the user is an ACTIVE person is checked on write.
async fn may_pick(state: &AppState, caller: Uuid, portfolio: Uuid, responsible: Uuid) -> Result<(), ApiError> {
    if responsible == caller {
        return Ok(());
    }
    let granted: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal_portfolio_grant WHERE principal_id = $1 AND portfolio_id = $2 AND can_view)",
    )
    .bind(responsible)
    .bind(portfolio)
    .fetch_one(state.pool())
    .await
    .map_err(db)?;
    if granted {
        Ok(())
    } else {
        Err(ApiError { status: StatusCode::UNPROCESSABLE_ENTITY, message: PICK_RESPONSIBLE.into() })
    }
}

/// Strategies on the portfolios where the caller holds `strategies.manage`.
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<StrategyRow>>, ApiError> {
    let books = authz::visible_portfolios(state.pool(), auth.principal_id, Permission::StrategiesManage)
        .await
        .map_err(db)?;
    Ok(Json(admin::list_rows(&state, Some(&books)).await?))
}

/// Same body as the admin create. The strategy always trades as a new
/// strategy principal of its own.
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(mut p): Json<CreateStrategy>,
) -> Result<Json<StrategyRow>, ApiError> {
    crate::authz::require_mfa(&auth)?;
    authz::require(state.pool(), auth.principal_id, Permission::StrategiesManage, p.portfolio_id).await?;
    may_pick(&state, auth.principal_id, p.portfolio_id, p.owner_principal_id).await?;
    p.principal_id = None;
    let row = admin::create_strategy_inner(state.pool(), p, &auth.principal_code).await?;
    admin::reload(&state).await;
    Ok(Json(row))
}

pub async fn handover(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(code): Path<String>,
    Json(p): Json<Handover>,
) -> Result<Json<StrategyRow>, ApiError> {
    crate::authz::require_mfa(&auth)?;
    let portfolio: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
        .bind(&code)
        .fetch_optional(state.pool())
        .await
        .map_err(db)?
        .ok_or_else(|| ApiError { status: StatusCode::NOT_FOUND, message: "strategy not found".into() })?;
    authz::require(state.pool(), auth.principal_id, Permission::StrategiesManage, portfolio).await?;
    may_pick(&state, auth.principal_id, portfolio, p.principal_id).await?;
    Ok(Json(admin::handover_inner(state.pool(), &code, p.principal_id, &auth.principal_code).await?))
}

#[cfg(test)]
mod tests {
    use axum::extract::{Extension, State};
    use axum::http::StatusCode;
    use axum::Json;

    use super::*;
    use crate::auth::AuthContext;
    use crate::strategies::test_support as ts;

    fn ctx(id: uuid::Uuid) -> Extension<AuthContext> {
        Extension(AuthContext::machine(id, "t".into()))
    }

    #[tokio::test]
    #[ignore]
    async fn pm_can_create_strategy_on_managed_portfolio() {
        let pool = ts::dev_pool().await;
        let state = ts::state(pool.clone());
        let portfolio = ts::portfolio(&pool).await;
        let pm = ts::user(&pool, "pm", "ACTIVE").await;
        ts::grant(&pool, pm, portfolio, true).await;
        let granted = ts::user(&pool, "trader", "ACTIVE").await;
        ts::grant(&pool, granted, portfolio, false).await;
        let stranger = ts::user(&pool, "trader", "ACTIVE").await;

        let err = create(State(state.clone()), ctx(pm), Json(ts::create_request(portfolio, stranger))).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let Json(own) = create(State(state.clone()), ctx(pm), Json(ts::create_request(portfolio, pm))).await.unwrap();
        assert_eq!(own.owner_principal_id, pm);
        // The one-portfolio rule: a second strategy needs its own portfolio.
        let other = ts::portfolio(&pool).await;
        ts::grant(&pool, pm, other, true).await;
        ts::grant(&pool, granted, other, false).await;
        let Json(theirs) = create(State(state.clone()), ctx(pm), Json(ts::create_request(other, granted))).await.unwrap();
        assert_eq!(theirs.owner_principal_id, granted);

        let Json(listed) = list(State(state), ctx(pm)).await.unwrap();
        let codes: Vec<_> = listed.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&own.code.as_str()) && codes.contains(&theirs.code.as_str()));
        assert!(listed.iter().all(|r| r.portfolio_id == portfolio || r.portfolio_id == other));
    }

    #[tokio::test]
    #[ignore]
    async fn trader_cannot_create_strategy() {
        let pool = ts::dev_pool().await;
        let state = ts::state(pool.clone());
        let portfolio = ts::portfolio(&pool).await;
        let trader = ts::user(&pool, "trader", "ACTIVE").await;
        ts::grant(&pool, trader, portfolio, true).await;
        let err = create(State(state), ctx(trader), Json(ts::create_request(portfolio, trader))).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[ignore]
    async fn pm_cannot_create_on_unmanaged_portfolio() {
        let pool = ts::dev_pool().await;
        let state = ts::state(pool.clone());
        let mine = ts::portfolio(&pool).await;
        let theirs = ts::portfolio(&pool).await;
        let pm = ts::user(&pool, "pm", "ACTIVE").await;
        ts::grant(&pool, pm, mine, true).await;
        let err = create(State(state), ctx(pm), Json(ts::create_request(theirs, pm))).await.unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }
}
