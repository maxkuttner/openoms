//! A portfolio's access list: groups and single people, each at view, trade
//! or manage. A person's effective access is the highest across their groups
//! and their own row (the `portfolio_access` view combines them).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::groups_api::{announce, Access};
use crate::handlers::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Group,
    Person,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AccessEntry {
    pub kind: EntryKind,
    pub id: Uuid,
    #[serde(default)]
    pub name: String,
    pub access: Access,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn db(err: sqlx::Error) -> ApiError {
    match &err {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown group or person")
        }
        sqlx::Error::Database(d) if d.is_unique_violation() => api_error(StatusCode::UNPROCESSABLE_ENTITY, "an entry is listed twice"),
        _ => crate::handlers::internal("database error", err),
    }
}

async fn entries(conn: &mut sqlx::PgConnection, portfolio: Uuid) -> Result<Vec<AccessEntry>, ApiError> {
    let rows = sqlx::query(
        "SELECT 'group' AS kind, g.id, g.name, gp.access FROM user_group_portfolio gp JOIN user_group g ON g.id = gp.group_id \
          WHERE gp.portfolio_id = $1 \
         UNION ALL \
         SELECT 'person', p.id, COALESCE(p.display_name, p.code), pa.access FROM portfolio_person_access pa JOIN principal p ON p.id = pa.principal_id \
          WHERE pa.portfolio_id = $1 \
         ORDER BY 1, 3",
    )
    .bind(portfolio)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    Ok(rows
        .iter()
        .map(|r| AccessEntry {
            kind: if r.get::<String, _>("kind") == "group" { EntryKind::Group } else { EntryKind::Person },
            id: r.get("id"),
            name: r.get("name"),
            access: Access::parse(r.get::<String, _>("access").as_str()),
        })
        .collect())
}

async fn require_portfolio(conn: &mut sqlx::PgConnection, id: Uuid, lock: bool) -> Result<(), ApiError> {
    let sql = if lock { "SELECT id FROM portfolio WHERE id = $1 FOR UPDATE" } else { "SELECT id FROM portfolio WHERE id = $1" };
    let found: Option<Uuid> = sqlx::query_scalar(sql).bind(id).fetch_optional(conn).await.map_err(db)?;
    found.map(|_| ()).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "portfolio not found"))
}

/// Everyone the access list reaches: group members and listed people.
async fn reached(conn: &mut sqlx::PgConnection, portfolio: Uuid) -> Result<Vec<Uuid>, ApiError> {
    sqlx::query_scalar(
        "SELECT m.principal_id FROM user_group_member m JOIN user_group_portfolio gp ON gp.group_id = m.group_id WHERE gp.portfolio_id = $1 \
         UNION SELECT principal_id FROM portfolio_person_access WHERE portfolio_id = $1",
    )
    .bind(portfolio)
    .fetch_all(conn)
    .await
    .map_err(db)
}

/// `GET /admin/portfolios/:id/access`
pub async fn get_access(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Vec<AccessEntry>>, ApiError> {
    let mut conn = state.pool().acquire().await.map_err(db)?;
    require_portfolio(&mut conn, id, false).await?;
    Ok(Json(entries(&mut conn, id).await?))
}

/// `PUT /admin/portfolios/:id/access`: exactly this list (groups and people).
pub async fn put_access(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(list): Json<Vec<AccessEntry>>,
) -> Result<StatusCode, ApiError> {
    let mut seen = std::collections::HashSet::new();
    if !list.iter().all(|e| seen.insert((e.kind, e.id))) {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "an entry is listed twice"));
    }
    let mut tx = state.pool().begin().await.map_err(db)?;
    require_portfolio(&mut tx, id, true).await?;
    // A strategy group keeps its strategy portfolio (as groups_api::set_portfolios).
    let keep: Vec<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM user_group g JOIN strategy s ON s.code = g.strategy_code \
          JOIN user_group_portfolio gp ON gp.group_id = g.id AND gp.portfolio_id = s.portfolio_id \
          WHERE s.portfolio_id = $1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    if !keep.iter().all(|g| list.iter().any(|e| e.kind == EntryKind::Group && e.id == *g)) {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "strategy group keeps its portfolio"));
    }
    let mut affected = reached(&mut tx, id).await?;
    sqlx::query("DELETE FROM user_group_portfolio WHERE portfolio_id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("DELETE FROM portfolio_person_access WHERE portfolio_id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    for e in &list {
        let sql = match e.kind {
            EntryKind::Group => "INSERT INTO user_group_portfolio (portfolio_id, group_id, access) VALUES ($1, $2, $3)",
            EntryKind::Person => "INSERT INTO portfolio_person_access (portfolio_id, principal_id, access) VALUES ($1, $2, $3)",
        };
        sqlx::query(sql).bind(id).bind(e.id).bind(e.access.as_str()).execute(&mut *tx).await.map_err(db)?;
    }
    let detail: Vec<_> = list.iter().map(|e| serde_json::json!({ "kind": e.kind, "id": e.id, "access": e.access })).collect();
    sqlx::query("INSERT INTO audit_event (actor, action, detail) VALUES ($1, 'portfolio.access_set', $2)")
        .bind(&ctx.principal_code)
        .bind(serde_json::json!({ "portfolio_id": id, "detail": detail }))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    affected.extend(reached(&mut tx, id).await?);
    tx.commit().await.map_err(db)?;
    affected.sort();
    affected.dedup();
    announce(&state, affected);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{allowed, Permission};
    use crate::authz_schema_tests::migrated_db;
    use crate::groups_api::tests::{admin, person, portfolio, state};

    fn entry(kind: EntryKind, id: Uuid, access: Access) -> AccessEntry {
        AccessEntry { kind, id, name: String::new(), access }
    }

    async fn new_group(pool: &sqlx::PgPool, members: &[Uuid]) -> Uuid {
        let g = Uuid::new_v4();
        sqlx::query("INSERT INTO user_group (id, name) VALUES ($1, $2)").bind(g).bind(format!("g-{}", g.simple())).execute(pool).await.unwrap();
        for m in members {
            sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)").bind(g).bind(m).execute(pool).await.unwrap();
        }
        g
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn person_and_group_access_highest_wins() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let g = new_group(&pool, &[me]).await;
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Group, g, Access::View), entry(EntryKind::Person, me, Access::Trade)]))
            .await
            .unwrap();
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap(), "person trade beats group view");
        assert!(!allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap());
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Group, g, Access::Manage), entry(EntryKind::Person, me, Access::View)]))
            .await
            .unwrap();
        assert!(allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap(), "group manage beats person view");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio_access WHERE principal_id = $1").bind(me).fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1, "one effective row per portfolio");
        let other = person(&pool).await;
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Person, other, Access::View)])).await.unwrap();
        assert!(allowed(&pool, other, Permission::PositionsView, pf).await.unwrap());
        assert!(!allowed(&pool, other, Permission::OrdersCreate, pf).await.unwrap());
        assert!(!allowed(&pool, me, Permission::PositionsView, pf).await.unwrap(), "removed from the list");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn put_access_replaces_both_kinds() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (a, b, pf) = (person(&pool).await, person(&pool).await, portfolio(&pool).await);
        let (g1, g2) = (new_group(&pool, &[]).await, new_group(&pool, &[]).await);
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Group, g1, Access::View), entry(EntryKind::Person, a, Access::Trade)]))
            .await
            .unwrap();
        let want = vec![entry(EntryKind::Group, g2, Access::Manage), entry(EntryKind::Person, b, Access::View)];
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(want.clone())).await.unwrap();
        let got: Vec<_> = get_access(State(st.clone()), Path(pf)).await.unwrap().0.into_iter().map(|e| (e.kind, e.id, e.access)).collect();
        assert_eq!(got, want.iter().map(|e| (e.kind, e.id, e.access)).collect::<Vec<_>>());
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'portfolio.access_set'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 2);
        let dup = put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Person, b, Access::View), entry(EntryKind::Person, b, Access::Trade)]))
            .await
            .unwrap_err();
        assert_eq!(dup.status, StatusCode::UNPROCESSABLE_ENTITY);
        let missing = get_access(State(st.clone()), Path(Uuid::new_v4())).await.unwrap_err();
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn strategy_group_row_is_kept() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (owner, pf) = (person(&pool).await, portfolio(&pool).await);
        let code = format!("S{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, 's', $2, $3, 'REPORTED')")
            .bind(&code).bind(pf).bind(owner).execute(&pool).await.unwrap();
        let g = Uuid::new_v4();
        sqlx::query("INSERT INTO user_group (id, name, strategy_code) VALUES ($1, $2, $3)").bind(g).bind(format!("strategy {code}")).bind(&code).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, 'manage')").bind(g).bind(pf).execute(&pool).await.unwrap();
        let other = person(&pool).await;
        let err = put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Person, other, Access::View)])).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.message, "strategy group keeps its portfolio");
        put_access(State(st.clone()), Extension(admin()), Path(pf), Json(vec![entry(EntryKind::Group, g, Access::Trade), entry(EntryKind::Person, other, Access::View)]))
            .await
            .unwrap();
        assert_eq!(get_access(State(st.clone()), Path(pf)).await.unwrap().0.len(), 2);
    }
}
