//! Grants in the database and the admin API that edits them.
//!
//! A venue grant also maintains the hidden `account` row `(portfolio,
//! connection)` that orders, fills and recon route through.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Serialize;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use super::Grant;
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::ApiError;

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn db(err: sqlx::Error) -> ApiError {
    match &err {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown connection")
        }
        _ => crate::handlers::internal("database error", err),
    }
}

fn params(g: &Grant) -> serde_json::Value {
    serde_json::to_value(g).expect("grant serializes")["params"].clone()
}

/// The portfolio's active grants, every kind.
pub async fn active_for_portfolio(conn: &mut PgConnection, portfolio: Uuid) -> sqlx::Result<Vec<Grant>> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT kind, params FROM trading_grant \
         WHERE subject_type = 'portfolio' AND subject_id = $1 AND status = 'ACTIVE' ORDER BY created_at, id",
    )
    .bind(portfolio)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(kind, params)| {
            serde_json::from_value(serde_json::json!({ "kind": kind, "params": params }))
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))
        })
        .collect()
}

/// Validate a venue grant against its connection; returns the normalized
/// grant (`account_ref` only for IBKR).
async fn normalize_venue(tx: &mut Transaction<'_, Postgres>, connection: &str, account_ref: Option<String>) -> Result<Grant, ApiError> {
    let broker: Option<String> = sqlx::query_scalar("SELECT broker_code FROM broker_connection WHERE code = $1")
        .bind(connection)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    let broker = broker.ok_or_else(|| api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown connection"))?;
    let account_ref = if matches!(broker.as_str(), "IBKR" | "IBKR_TWS") {
        let r = account_ref.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
        Some(r.ok_or_else(|| api_error(StatusCode::UNPROCESSABLE_ENTITY, "IBKR needs its account"))?)
    } else {
        None
    };
    Ok(Grant::Venue { connection: connection.to_string(), account_ref })
}

/// Replace the portfolio's active grants with `grants`: new ones are inserted
/// or reactivated, missing ones revoked. Venue grants keep the hidden
/// account row in step. Audited as `grants.set`.
pub async fn set_for_portfolio(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &AuthContext,
    portfolio: Uuid,
    grants: Vec<Grant>,
) -> Result<(), ApiError> {
    let code: Option<String> = sqlx::query_scalar("SELECT code FROM portfolio WHERE id = $1 FOR UPDATE")
        .bind(portfolio)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    let code = code.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "portfolio not found"))?;

    let mut wanted: Vec<Grant> = Vec::new();
    for g in grants {
        let g = match g {
            Grant::Venue { connection, account_ref } => {
                if wanted.iter().any(|w| matches!(w, Grant::Venue { connection: c, .. } if *c == connection)) {
                    return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "venue granted twice"));
                }
                normalize_venue(tx, &connection, account_ref).await?
            }
        };
        wanted.push(g);
    }
    let active = active_for_portfolio(&mut **tx, portfolio).await.map_err(db)?;

    for g in active.iter().filter(|g| !wanted.contains(g)) {
        sqlx::query(
            "UPDATE trading_grant SET status = 'REVOKED', updated_at = now() \
             WHERE subject_type = 'portfolio' AND subject_id = $1 AND kind = $2 AND params = $3",
        )
        .bind(portfolio)
        .bind(g.kind())
        .bind(params(g))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        match g {
            Grant::Venue { connection, .. } => {
                // A changed IBKR account keeps the venue: the add below updates the row.
                let kept = wanted.iter().any(|w| matches!(w, Grant::Venue { connection: c, .. } if c == connection));
                if !kept {
                    close_venue(tx, portfolio, connection).await?;
                }
            }
        }
    }

    for g in wanted.iter().filter(|g| !active.contains(g)) {
        sqlx::query(
            "INSERT INTO trading_grant (subject_type, subject_id, kind, params) VALUES ('portfolio', $1, $2, $3) \
             ON CONFLICT (subject_type, subject_id, kind, params) DO UPDATE SET status = 'ACTIVE', updated_at = now()",
        )
        .bind(portfolio)
        .bind(g.kind())
        .bind(params(g))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        match g {
            Grant::Venue { connection, account_ref } => {
                sqlx::query(
                    "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
                     VALUES ($1, $2, $3, $4, 'ACTIVE', $5) \
                     ON CONFLICT (portfolio_id, broker_connection_code) WHERE portfolio_id IS NOT NULL \
                     DO UPDATE SET status = 'ACTIVE', external_account_ref = EXCLUDED.external_account_ref, updated_at = now()",
                )
                .bind(Uuid::new_v4())
                .bind(format!("{code}-{connection}"))
                .bind(connection)
                .bind(account_ref.clone().unwrap_or_default())
                .bind(portfolio)
                .execute(&mut **tx)
                .await
                .map_err(db)?;
            }
        }
    }

    sqlx::query("INSERT INTO audit_event (actor, action, detail) VALUES ($1, 'grants.set', $2)")
        .bind(&ctx.principal_code)
        .bind(serde_json::json!({ "portfolio_id": portfolio, "grants": wanted }))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

/// Close the portfolio's hidden account on `connection`, unless a strategy uses it.
async fn close_venue(tx: &mut Transaction<'_, Postgres>, portfolio: Uuid, connection: &str) -> Result<(), ApiError> {
    let user: Option<String> = sqlx::query_scalar(
        "SELECT sc.strategy_code FROM strategy_connection sc JOIN account a ON a.id = sc.account_id \
         WHERE a.portfolio_id = $1 AND a.broker_connection_code = $2 ORDER BY 1 LIMIT 1",
    )
    .bind(portfolio)
    .bind(connection)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    if let Some(s) = user {
        return Err(api_error(StatusCode::CONFLICT, format!("venue is used by strategy {s}")));
    }
    sqlx::query("UPDATE account SET status = 'CLOSED', updated_at = now() WHERE portfolio_id = $1 AND broker_connection_code = $2")
        .bind(portfolio)
        .bind(connection)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Connection {
    pub code: String,
    pub broker_code: String,
    pub environment: String,
}

#[derive(Debug, Serialize)]
pub struct GrantsView {
    pub grants: Vec<Grant>,
    pub connections: Vec<Connection>,
}

/// `GET /admin/portfolios/:id/grants`: the active grants and, for the
/// editor, every broker connection.
pub async fn get_grants(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<GrantsView>, ApiError> {
    let mut conn = state.pool().acquire().await.map_err(db)?;
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM portfolio WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?;
    found.ok_or_else(|| api_error(StatusCode::NOT_FOUND, "portfolio not found"))?;
    let grants = active_for_portfolio(&mut conn, id).await.map_err(db)?;
    let connections = sqlx::query_as("SELECT code, broker_code, environment FROM broker_connection ORDER BY code")
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?;
    Ok(Json(GrantsView { grants, connections }))
}

/// `PUT /admin/portfolios/:id/grants`: replace the full active set. A body
/// that is not a list of valid grants is rejected (422) before any write.
pub async fn put_grants(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(grants): Json<Vec<Grant>>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    set_for_portfolio(&mut tx, &ctx, id, grants).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use crate::groups_api::tests::{admin, portfolio, state};
    use axum::extract::FromRequest;
    use sqlx::PgPool;

    async fn connection(pool: &PgPool, broker: &str) -> String {
        let code = format!("c-{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO broker_connection (code, broker_code) VALUES ($1, $2)")
            .bind(&code)
            .bind(format!("{broker}"))
            .execute(pool)
            .await
            .unwrap();
        code
    }

    fn venue(c: &str) -> Grant {
        Grant::Venue { connection: c.into(), account_ref: None }
    }

    async fn put(st: &AppState, pf: Uuid, grants: Vec<Grant>) -> Result<StatusCode, ApiError> {
        put_grants(State(st.clone()), Extension(admin()), Path(pf), Json(grants)).await
    }

    async fn get(st: &AppState, pf: Uuid) -> Vec<Grant> {
        get_grants(State(st.clone()), Path(pf)).await.unwrap().0.grants
    }

    /// (code, ref, status) of the portfolio's hidden account on a connection.
    async fn account(pool: &PgPool, pf: Uuid, conn: &str) -> Option<(String, String, String)> {
        sqlx::query_as("SELECT code, external_account_ref, status FROM account WHERE portfolio_id = $1 AND broker_connection_code = $2")
            .bind(pf)
            .bind(conn)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn pcode(pool: &PgPool, pf: Uuid) -> String {
        sqlx::query_scalar("SELECT code FROM portfolio WHERE id = $1").bind(pf).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn venue_grant_creates_account_row() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        let c = connection(&pool, "ALPACA").await;
        assert_eq!(put(&st, pf, vec![venue(&c)]).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(get(&st, pf).await, vec![venue(&c)]);
        let code = format!("{}-{c}", pcode(&pool, pf).await);
        assert_eq!(account(&pool, pf, &c).await, Some((code, String::new(), "ACTIVE".into())));
        let view = get_grants(State(st.clone()), Path(pf)).await.unwrap().0;
        assert!(view.connections.iter().any(|x| x.code == c && x.broker_code == "ALPACA" && x.environment == "PAPER"));
        let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'grants.set'").fetch_one(&pool).await.unwrap();
        assert_eq!(audited, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn two_portfolios_share_a_connection() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (a, b) = (portfolio(&pool).await, portfolio(&pool).await);
        let c = connection(&pool, "ALPACA").await;
        put(&st, a, vec![venue(&c)]).await.unwrap();
        put(&st, b, vec![venue(&c)]).await.unwrap();
        assert!(account(&pool, a, &c).await.is_some());
        assert!(account(&pool, b, &c).await.is_some());
        assert_eq!(get(&st, b).await, vec![venue(&c)]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn revoke_closes_account_row() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        let c = connection(&pool, "ALPACA").await;
        put(&st, pf, vec![venue(&c)]).await.unwrap();
        put(&st, pf, vec![]).await.unwrap();
        assert_eq!(get(&st, pf).await, vec![]);
        assert_eq!(account(&pool, pf, &c).await.unwrap().2, "CLOSED");
        let status: String = sqlx::query_scalar("SELECT status FROM trading_grant WHERE subject_id = $1").bind(pf).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "REVOKED", "history kept");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn regrant_reactivates_rows() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        let c = connection(&pool, "ALPACA").await;
        put(&st, pf, vec![venue(&c)]).await.unwrap();
        let first = account(&pool, pf, &c).await.unwrap();
        put(&st, pf, vec![]).await.unwrap();
        put(&st, pf, vec![venue(&c)]).await.unwrap();
        assert_eq!(account(&pool, pf, &c).await.unwrap(), first);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM trading_grant WHERE subject_id = $1").bind(pf).fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1);
        let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE portfolio_id = $1").bind(pf).fetch_one(&pool).await.unwrap();
        assert_eq!(accounts, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn ibkr_needs_account_ref() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        for broker in ["IBKR", "IBKR_TWS"] {
            let c = connection(&pool, broker).await;
            let err = put(&st, pf, vec![venue(&c)]).await.unwrap_err();
            assert_eq!((err.status, err.message.as_str()), (StatusCode::UNPROCESSABLE_ENTITY, "IBKR needs its account"));
            let blank = Grant::Venue { connection: c.clone(), account_ref: Some(" ".into()) };
            assert!(put(&st, pf, vec![blank]).await.is_err());
            let ok = Grant::Venue { connection: c.clone(), account_ref: Some("U123".into()) };
            put(&st, pf, vec![ok]).await.unwrap();
            assert_eq!(account(&pool, pf, &c).await.unwrap().1, "U123");
            put(&st, pf, vec![]).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cannot_revoke_venue_used_by_strategy() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (pf, owner) = (portfolio(&pool).await, crate::groups_api::tests::person(&pool).await);
        let c = connection(&pool, "ALPACA").await;
        put(&st, pf, vec![venue(&c)]).await.unwrap();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM account WHERE portfolio_id = $1").bind(pf).fetch_one(&pool).await.unwrap();
        let s = format!("s-{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, $1, $2, $3, 'REPORTED')")
            .bind(&s).bind(pf).bind(owner).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO strategy_connection (strategy_code, broker_code, account_id) VALUES ($1, 'ALPACA', $2)")
            .bind(&s).bind(id).execute(&pool).await.unwrap();
        let err = put(&st, pf, vec![]).await.unwrap_err();
        assert_eq!((err.status, err.message), (StatusCode::CONFLICT, format!("venue is used by strategy {s}")));
        assert_eq!(get(&st, pf).await, vec![venue(&c)]);
        assert_eq!(account(&pool, pf, &c).await.unwrap().2, "ACTIVE");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn bad_grant_is_rejected_atomically() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        let c = connection(&pool, "ALPACA").await;
        put(&st, pf, vec![venue(&c)]).await.unwrap();
        for body in [r#"[{"kind":"teleport"}]"#, r#"[{"kind":"venue","params":{}}]"#] {
            let req = axum::http::Request::builder()
                .method("PUT")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap();
            let rejected = Json::<Vec<Grant>>::from_request(req, &()).await.unwrap_err();
            use axum::response::IntoResponse;
            assert_eq!(rejected.into_response().status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
        // An unknown connection fails inside the transaction; nothing changes.
        assert!(put(&st, pf, vec![venue("nope")]).await.is_err());
        assert_eq!(get(&st, pf).await, vec![venue(&c)]);
        assert_eq!(account(&pool, pf, &c).await.unwrap().2, "ACTIVE");
    }
}
