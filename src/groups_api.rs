//! Admin API for groups: the way people get portfolio rights (Open WebUI's
//! groups). A group has members and, per portfolio, an access level: `view`
//! (viewer permissions), `trade` (trader) or `manage` (PM). A person's access
//! on a portfolio is the highest across their groups (the `portfolio_access`
//! view, which `authz` reads). Every change is audited and announced with
//! `AuthChanged` for each person whose access may have moved, so open
//! sessions and sockets pick it up on their next request.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::ApiError;

/// A portfolio access level, lowest to highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    View,
    Trade,
    Manage,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::View => "view",
            Access::Trade => "trade",
            Access::Manage => "manage",
        }
    }

    fn parse(s: &str) -> Access {
        match s {
            "manage" => Access::Manage,
            "trade" => Access::Trade,
            _ => Access::View,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GroupPortfolio {
    pub portfolio_id: Uuid,
    #[serde(default, skip_deserializing)]
    pub code: String,
    pub access: Access,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub members: Vec<Uuid>,
    pub portfolios: Vec<GroupPortfolio>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GroupBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn db(err: sqlx::Error) -> ApiError {
    match &err {
        sqlx::Error::Database(d) if d.is_unique_violation() => api_error(StatusCode::CONFLICT, "a group with this name exists"),
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
            api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown user or portfolio")
        }
        _ => crate::handlers::internal("database error", err),
    }
}

async fn audit(tx: &mut Transaction<'_, Postgres>, ctx: &AuthContext, action: &str, group: Uuid, detail: serde_json::Value) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO audit_event (actor, action, detail) VALUES ($1, $2, $3)")
        .bind(&ctx.principal_code)
        .bind(action)
        .bind(serde_json::json!({ "group_id": group, "detail": detail }))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

async fn members_of(tx: &mut Transaction<'_, Postgres>, group: Uuid) -> Result<Vec<Uuid>, ApiError> {
    sqlx::query_scalar("SELECT principal_id FROM user_group_member WHERE group_id = $1")
        .bind(group)
        .fetch_all(&mut **tx)
        .await
        .map_err(db)
}

/// 404 unless the group exists; locks it for the rest of the transaction.
async fn lock_group(tx: &mut Transaction<'_, Postgres>, group: Uuid) -> Result<(), ApiError> {
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM user_group WHERE id = $1 FOR UPDATE")
        .bind(group)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    found.map(|_| ()).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "group not found"))
}

fn announce(state: &AppState, who: impl IntoIterator<Item = Uuid>) {
    for id in who {
        state.event_bus().publish_auth_changed(id);
    }
}

/// `GET /admin/groups`, by name.
pub async fn list_groups(State(state): State<AppState>) -> Result<Json<Vec<Group>>, ApiError> {
    let pool = state.pool();
    let groups = sqlx::query("SELECT id, name, description FROM user_group ORDER BY lower(name), name")
        .fetch_all(pool)
        .await
        .map_err(db)?;
    let members = sqlx::query("SELECT group_id, principal_id FROM user_group_member").fetch_all(pool).await.map_err(db)?;
    let access = sqlx::query(
        "SELECT gp.group_id, gp.portfolio_id, p.code, gp.access FROM user_group_portfolio gp \
           JOIN portfolio p ON p.id = gp.portfolio_id ORDER BY p.code",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(Json(
        groups
            .into_iter()
            .map(|g| {
                let id: Uuid = g.get("id");
                Group {
                    id,
                    name: g.get("name"),
                    description: g.get("description"),
                    members: members.iter().filter(|m| m.get::<Uuid, _>("group_id") == id).map(|m| m.get("principal_id")).collect(),
                    portfolios: access
                        .iter()
                        .filter(|a| a.get::<Uuid, _>("group_id") == id)
                        .map(|a| GroupPortfolio {
                            portfolio_id: a.get("portfolio_id"),
                            code: a.get("code"),
                            access: Access::parse(a.get("access")),
                        })
                        .collect(),
                }
            })
            .collect(),
    ))
}

fn require_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "name is required"));
    }
    Ok(name)
}

/// `POST /admin/groups`: an empty group.
pub async fn create_group(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(p): Json<GroupBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let name = require_name(&p.name)?;
    let id = Uuid::new_v4();
    let mut tx = state.pool().begin().await.map_err(db)?;
    sqlx::query("INSERT INTO user_group (id, name, description) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(p.description.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    audit(&mut tx, &ctx, "group.created", id, serde_json::json!({ "name": name })).await?;
    tx.commit().await.map_err(db)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

/// `PUT /admin/groups/:id`: rename, or change the description.
pub async fn update_group(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(p): Json<GroupBody>,
) -> Result<StatusCode, ApiError> {
    let name = require_name(&p.name)?;
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    sqlx::query("UPDATE user_group SET name = $2, description = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(name)
        .bind(p.description.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    audit(&mut tx, &ctx, "group.updated", id, serde_json::json!({ "name": name })).await?;
    tx.commit().await.map_err(db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/groups/:id`: its members lose what it gave them.
pub async fn delete_group(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    let affected = members_of(&mut tx, id).await?;
    sqlx::query("DELETE FROM user_group WHERE id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    audit(&mut tx, &ctx, "group.deleted", id, serde_json::json!({ "members": affected })).await?;
    tx.commit().await.map_err(db)?;
    announce(&state, affected);
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /admin/groups/:id/members`: exactly these people (HUMAN only).
pub async fn set_members(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(members): Json<Vec<Uuid>>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    let humans: i64 = sqlx::query_scalar("SELECT count(*) FROM principal WHERE id = ANY($1) AND principal_type = 'HUMAN'")
        .bind(&members)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let mut unique = members.clone();
    unique.sort();
    unique.dedup();
    if humans != unique.len() as i64 {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "members must be people"));
    }
    let before = members_of(&mut tx, id).await?;
    sqlx::query("DELETE FROM user_group_member WHERE group_id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    for m in &unique {
        sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)")
            .bind(id)
            .bind(m)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    audit(&mut tx, &ctx, "group.members_changed", id, serde_json::json!({ "from": before, "to": unique })).await?;
    tx.commit().await.map_err(db)?;
    announce(&state, before.into_iter().chain(unique));
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /admin/groups/:id/portfolios`: exactly this access table.
pub async fn set_portfolios(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(access): Json<Vec<GroupPortfolio>>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    sqlx::query("DELETE FROM user_group_portfolio WHERE group_id = $1").bind(id).execute(&mut *tx).await.map_err(db)?;
    for a in &access {
        sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(a.portfolio_id)
            .bind(a.access.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.is_unique_violation() => api_error(StatusCode::UNPROCESSABLE_ENTITY, "a portfolio is listed twice"),
                _ => db(e),
            })?;
    }
    let detail: Vec<_> = access.iter().map(|a| serde_json::json!({ "portfolio_id": a.portfolio_id, "access": a.access })).collect();
    audit(&mut tx, &ctx, "group.access_changed", id, serde_json::json!(detail)).await?;
    let affected = members_of(&mut tx, id).await?;
    tx.commit().await.map_err(db)?;
    announce(&state, affected);
    Ok(StatusCode::NO_CONTENT)
}

/// Put `principal` in exactly `groups` (used by approve and the Users page).
/// Returns nothing to announce: the caller publishes for `principal`.
pub async fn set_user_groups(tx: &mut Transaction<'_, Postgres>, principal: Uuid, groups: &[Uuid]) -> Result<(), ApiError> {
    sqlx::query("DELETE FROM user_group_member WHERE principal_id = $1").bind(principal).execute(&mut **tx).await.map_err(db)?;
    let mut unique = groups.to_vec();
    unique.sort();
    unique.dedup();
    for g in &unique {
        sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)")
            .bind(g)
            .bind(principal)
            .execute(&mut **tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(d) if d.is_foreign_key_violation() => api_error(StatusCode::UNPROCESSABLE_ENTITY, "unknown group"),
                _ => db(e),
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{allowed, Permission};
    use crate::authz_schema_tests::migrated_db;
    use sqlx::PgPool;

    fn admin() -> AuthContext {
        AuthContext::machine(Uuid::nil(), "ada".to_string())
    }

    async fn person(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(id)
            .bind(format!("u-{}", id.simple()))
            .execute(pool)
            .await
            .unwrap();
        id
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

    async fn group(st: &AppState, name: &str, members: &[Uuid], access: &[(Uuid, Access)]) -> Uuid {
        let (status, out) = create_group(State(st.clone()), Extension(admin()), Json(GroupBody { name: name.into(), description: None }))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let id: Uuid = out.0["id"].as_str().unwrap().parse().unwrap();
        set_members(State(st.clone()), Extension(admin()), Path(id), Json(members.to_vec())).await.unwrap();
        let table = access.iter().map(|&(portfolio_id, access)| GroupPortfolio { portfolio_id, code: String::new(), access }).collect();
        set_portfolios(State(st.clone()), Extension(admin()), Path(id), Json(table)).await.unwrap();
        id
    }

    fn state(pool: PgPool) -> AppState {
        crate::handlers::tests::test_app_state(pool)
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn group_access_max_across_groups() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        group(&st, "viewers", &[me], &[(pf, Access::View)]).await;
        assert!(allowed(&pool, me, Permission::PositionsView, pf).await.unwrap());
        assert!(!allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap(), "view does not trade");
        assert!(!allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap());

        group(&st, "traders", &[me], &[(pf, Access::Trade)]).await;
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap(), "the higher group wins");
        assert!(!allowed(&pool, me, Permission::OrdersCancelAny, pf).await.unwrap(), "trade is not manage");

        let pms = group(&st, "pms", &[me], &[(pf, Access::Manage)]).await;
        assert!(allowed(&pool, me, Permission::OrdersCancelAny, pf).await.unwrap());
        assert!(allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap());
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio_access WHERE principal_id = $1").bind(me).fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1, "one effective row per portfolio");

        delete_group(State(st.clone()), Extension(admin()), Path(pms)).await.unwrap();
        assert!(!allowed(&pool, me, Permission::LimitsManage, pf).await.unwrap(), "back to trade");
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap());
        let other = portfolio(&pool).await;
        assert!(!allowed(&pool, me, Permission::PositionsView, other).await.unwrap(), "deny by default");
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres (seeded); run with --ignored"]
    async fn trader_via_group_can_submit_viewer_via_group_cannot() {
        let pool = crate::handlers::tests::test_pool().await;
        let st = state(pool.clone());
        let (pf, instrument) = crate::authz_route_tests::tradeable_portfolio(&pool).await;
        let (trader, viewer) = (person(&pool).await, person(&pool).await);
        let suffix = Uuid::new_v4().simple().to_string();
        group(&st, &format!("t-{suffix}"), &[trader], &[(pf, Access::Trade)]).await;
        group(&st, &format!("v-{suffix}"), &[viewer], &[(pf, Access::View)]).await;
        let ctx = |id: Uuid| AuthContext { principal_id: id, principal_code: "x".into(), auth_method: crate::auth::AuthMethod::Oidc, mfa: true };
        let (_, status) = crate::authz_route_tests::submit(&pool, &ctx(trader), pf, instrument).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "authorized; only routing fails");
        let (_, status) = crate::authz_route_tests::submit(&pool, &ctx(viewer), pf, instrument).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn removing_user_from_group_revokes_access_next_request_and_closes_socket() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let g = group(&st, "desk", &[me], &[(pf, Access::Trade)]).await;
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap());
        // The live-updates socket of this person reloads its scope on AuthChanged.
        let mut auth = st.event_bus().subscribe_auth();
        set_members(State(st.clone()), Extension(admin()), Path(g), Json(vec![])).await.unwrap();
        assert!(!allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap(), "the next request is refused");
        assert!(!allowed(&pool, me, Permission::PositionsView, pf).await.unwrap());
        assert_eq!(auth.try_recv().unwrap().principal_id, me, "AuthChanged for the removed member");
        let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'group.members_changed'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(audited, 2);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn existing_grants_migrated_to_per_user_groups() {
        use crate::setup::database::migrate;
        let admin_pool = crate::authz_schema_tests::fresh_db().await;
        migrate::ensure_tracking(&admin_pool).await.unwrap();
        migrate::apply_matching(&admin_pool, |schema, f| schema != "oms" || f < "0064_").await.unwrap();
        let pool = admin_pool.clone();
        sqlx::query("SET search_path TO oms, public").execute(&pool).await.unwrap();
        let q = |sql: &'static str| sqlx::query(sql);
        let (pm, trader, viewer_ro) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (pf1, pf2) = (Uuid::new_v4(), Uuid::new_v4());
        for (id, code, role) in [(pm, "mig-pm", "pm"), (trader, "mig-tr", "trader"), (viewer_ro, "mig-ro", "trader")] {
            q("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')").bind(id).bind(code).execute(&pool).await.unwrap();
            q("INSERT INTO oms.principal_role (principal_id, role_code) VALUES ($1, $2)").bind(id).bind(role).execute(&pool).await.unwrap();
        }
        for (id, code) in [(pf1, "mig-p1"), (pf2, "mig-p2")] {
            q("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, $2, 'p', 'ACTIVE')").bind(id).bind(code).execute(&pool).await.unwrap();
        }
        for (who, pf, trade) in [(pm, pf1, true), (pm, pf2, false), (trader, pf1, true), (viewer_ro, pf2, false)] {
            q("INSERT INTO oms.principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) VALUES ($1, $2, $3, $4, true, $4)")
                .bind(Uuid::new_v4()).bind(who).bind(pf).bind(trade).execute(&pool).await.unwrap();
        }
        migrate::apply_all(&admin_pool).await.unwrap();

        let access = |who: Uuid, pf: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT gp.access FROM oms.user_group_member m JOIN oms.user_group_portfolio gp USING (group_id) \
                      WHERE m.principal_id = $1 AND gp.portfolio_id = $2",
                )
                .bind(who)
                .bind(pf)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(access(pm, pf1).await, "manage");
        assert_eq!(access(pm, pf2).await, "view");
        assert_eq!(access(trader, pf1).await, "trade");
        assert_eq!(access(viewer_ro, pf2).await, "view");
        let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.user_group").fetch_one(&pool).await.unwrap();
        assert_eq!(groups, 3, "one group per person");
        let direct: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.principal_portfolio_grant").fetch_one(&pool).await.unwrap();
        assert_eq!(direct, 0, "people's direct grants moved into their groups");
        assert!(allowed(&pool, trader, Permission::OrdersCreate, pf1).await.unwrap(), "nobody lost access");
        assert!(allowed(&pool, pm, Permission::OrdersCancelAny, pf1).await.unwrap());
    }
}
