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
    /// Set when a reported strategy owns the group.
    pub strategy_code: Option<String>,
    /// The owning strategy is archived, so the group may be deleted.
    pub strategy_archived: bool,
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

pub(crate) async fn audit(tx: &mut Transaction<'_, Postgres>, ctx: &AuthContext, action: &str, group: Uuid, detail: serde_json::Value) -> Result<(), ApiError> {
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

/// For a strategy-owned group: the strategy's owner and portfolio. `None`
/// for an ordinary group.
async fn strategy_of(tx: &mut Transaction<'_, Postgres>, group: Uuid) -> Result<Option<(Uuid, Uuid)>, ApiError> {
    sqlx::query_as(
        "SELECT s.owner_principal_id, s.portfolio_id FROM user_group g JOIN strategy s ON s.code = g.strategy_code WHERE g.id = $1",
    )
    .bind(group)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)
}

fn owner_stays() -> ApiError {
    api_error(StatusCode::UNPROCESSABLE_ENTITY, "owner must stay in strategy group")
}

fn announce(state: &AppState, who: impl IntoIterator<Item = Uuid>) {
    for id in who {
        state.event_bus().publish_auth_changed(id);
    }
}

/// `GET /admin/groups`, by name.
pub async fn list_groups(State(state): State<AppState>) -> Result<Json<Vec<Group>>, ApiError> {
    let pool = state.pool();
    let groups = sqlx::query("SELECT g.id, g.name, g.description, g.strategy_code, s.archived_at IS NOT NULL AS strategy_archived \
           FROM user_group g LEFT JOIN strategy s ON s.code = g.strategy_code ORDER BY lower(g.name), g.name")
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
                    strategy_code: g.get("strategy_code"),
                    strategy_archived: g.get("strategy_archived"),
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

/// `DELETE /admin/groups/:id`: its members lose what it gave them. A strategy's
/// group only once the strategy is archived.
pub async fn delete_group(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    let live_strategy: Option<(bool,)> = sqlx::query_as(
        "SELECT s.archived_at IS NULL FROM user_group g JOIN strategy s ON s.code = g.strategy_code WHERE g.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if live_strategy.is_some_and(|(live,)| live) {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "a strategy group cannot be deleted until the strategy is archived"));
    }
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
    if let Some((owner, _)) = strategy_of(&mut tx, id).await? {
        if !unique.contains(&owner) {
            return Err(owner_stays());
        }
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
    let mut seen = std::collections::HashSet::new();
    if !access.iter().all(|a| seen.insert(a.portfolio_id)) {
        return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "a portfolio is listed twice"));
    }
    let mut tx = state.pool().begin().await.map_err(db)?;
    lock_group(&mut tx, id).await?;
    if let Some((_, portfolio)) = strategy_of(&mut tx, id).await? {
        if !access.iter().any(|a| a.portfolio_id == portfolio) {
            return Err(api_error(StatusCode::UNPROCESSABLE_ENTITY, "a strategy group must keep its strategy portfolio"));
        }
    }
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
    let mut unique = groups.to_vec();
    unique.sort();
    unique.dedup();
    // A person keeps the strategy groups of the strategies they own.
    let owned: Vec<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM user_group g JOIN strategy s ON s.code = g.strategy_code WHERE s.owner_principal_id = $1",
    )
    .bind(principal)
    .fetch_all(&mut **tx)
    .await
    .map_err(db)?;
    if owned.iter().any(|g| !unique.contains(g)) {
        return Err(owner_stays());
    }
    sqlx::query("DELETE FROM user_group_member WHERE principal_id = $1").bind(principal).execute(&mut **tx).await.map_err(db)?;
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
pub(crate) mod tests {
    use super::*;
    use crate::authz::{allowed, Permission};
    use crate::authz_schema_tests::migrated_db;
    use sqlx::PgPool;

    pub(crate) fn admin() -> AuthContext {
        AuthContext::machine(Uuid::nil(), "ada".to_string())
    }

    pub(crate) async fn person(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(id)
            .bind(format!("u-{}", id.simple()))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    pub(crate) async fn portfolio(pool: &PgPool) -> Uuid {
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

    pub(crate) fn state(pool: PgPool) -> AppState {
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
    async fn removing_user_from_group_revokes_access_and_reloads_socket_scope() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let g = group(&st, "desk", &[me], &[(pf, Access::Trade)]).await;
        assert!(allowed(&pool, me, Permission::OrdersCreate, pf).await.unwrap());
        let scope = crate::live_updates::StreamScope::load(&pool, me).await.unwrap();
        assert!(scope.sees_positions(pf), "the socket scope includes the portfolio");
        // The live-updates socket of this person reloads its scope on AuthChanged.
        let mut auth = st.event_bus().subscribe_auth();
        set_members(State(st.clone()), Extension(admin()), Path(g), Json(vec![])).await.unwrap();
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM user_group_member WHERE group_id = $1").bind(g).fetch_one(&pool).await.unwrap();
        assert_eq!(members, 0, "an empty body clears the members");
        let reloaded = crate::live_updates::StreamScope::load(&pool, me).await.unwrap();
        assert!(!reloaded.sees_positions(pf), "the reloaded socket scope drops it");
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
        // Unqualified names (as in `allowed`) must resolve in `oms` on every pooled
        // connection, not just the one a `SET search_path` happened to run on.
        let name: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&admin_pool).await.unwrap();
        sqlx::query(&format!("ALTER DATABASE {name} SET search_path = oms, public")).execute(&admin_pool).await.unwrap();
        let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL").unwrap();
        let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        let pool = sqlx::PgPool::connect_with(options.database(&name)).await.unwrap();
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
        let roles: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.principal_role r JOIN oms.principal p ON p.id = r.principal_id WHERE p.principal_type = 'HUMAN'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(roles, 0, "people's leftover roles are gone: groups decide");
    }

    /// Seed one person (with `role`, if any) holding one grant, migrated from
    /// just before 0064. Returns the pool and the person.
    async fn migrate_one_grant(role: Option<&str>, can_view: bool, can_trade: bool) -> (PgPool, Uuid, Uuid) {
        use crate::setup::database::migrate;
        let pool = crate::authz_schema_tests::fresh_db().await;
        migrate::ensure_tracking(&pool).await.unwrap();
        migrate::apply_matching(&pool, |schema, f| schema != "oms" || f < "0064_").await.unwrap();
        let (who, pf) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query("INSERT INTO oms.principal (id, code, principal_type, status) VALUES ($1, 'one', 'HUMAN', 'ACTIVE')").bind(who).execute(&pool).await.unwrap();
        if let Some(role) = role {
            sqlx::query("INSERT INTO oms.principal_role (principal_id, role_code) VALUES ($1, $2)").bind(who).bind(role).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, 'one-p', 'p', 'ACTIVE')").bind(pf).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO oms.principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) VALUES ($1, $2, $3, $4, $5)")
            .bind(Uuid::new_v4()).bind(who).bind(pf).bind(can_trade).bind(can_view).execute(&pool).await.unwrap();
        migrate::apply_all(&pool).await.unwrap();
        sqlx::query("SET search_path TO oms, public").execute(&pool).await.unwrap();
        (pool, who, pf)
    }

    async fn access_rows(pool: &PgPool, who: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM oms.portfolio_access WHERE principal_id = $1").bind(who).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn grant_without_role_migrates_to_no_access() {
        let (pool, who, pf) = migrate_one_grant(None, true, true).await;
        assert_eq!(access_rows(&pool, who).await, 0, "no role meant no permission before: still none");
        assert!(!allowed(&pool, who, Permission::PositionsView, pf).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn grant_without_view_is_dropped() {
        let (pool, who, _) = migrate_one_grant(Some("viewer"), false, false).await;
        assert_eq!(access_rows(&pool, who).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn a_repeated_portfolio_in_the_access_table_is_refused_cleanly() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pf = portfolio(&pool).await;
        let g = group(&st, "dup", &[], &[(pf, Access::View)]).await;
        let twice = vec![
            GroupPortfolio { portfolio_id: pf, code: String::new(), access: Access::Trade },
            GroupPortfolio { portfolio_id: pf, code: String::new(), access: Access::View },
        ];
        let err = set_portfolios(State(st.clone()), Extension(admin()), Path(g), Json(twice)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let kept: String = sqlx::query_scalar("SELECT access FROM user_group_portfolio WHERE group_id = $1").bind(g).fetch_one(&pool).await.unwrap();
        assert_eq!(kept, "view", "nothing changed");
    }

    /// Admin trading tokens never hand a person portfolio access outside groups.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn trading_token_refuses_portfolio_for_a_person() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        let body = crate::admin::CreateTradingToken { principal_id: me, portfolio_id: Some(pf), label: None };
        let Err(err) = crate::admin::create_trading_token(State(st.clone()), Json(body)).await else {
            panic!("a person's portfolio must be refused");
        };
        let resp = axum::response::IntoResponse::into_response(err);
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(access_rows(&pool, me).await, 0);
        let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM api_key WHERE principal_id = $1").bind(me).fetch_one(&pool).await.unwrap();
        assert_eq!(keys, 0, "nothing was created");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn portfolios_list_has_no_duplicates() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (me, pf) = (person(&pool).await, portfolio(&pool).await);
        group(&st, "a", &[me], &[(pf, Access::View)]).await;
        group(&st, "b", &[me], &[(pf, Access::Trade)]).await;
        // A direct grant on the same portfolio (only strategies get these now;
        // tests and old data may still hold one): still one row.
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'trader')").bind(me).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) VALUES ($1, $2, $3, true, true)")
            .bind(Uuid::new_v4()).bind(me).bind(pf).execute(&pool).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM portfolio_access WHERE principal_id = $1 AND portfolio_id = $2")
            .bind(me).bind(pf).fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1, "one access row per (principal, portfolio)");
        let ctx = AuthContext { principal_id: me, principal_code: "x".into(), auth_method: crate::auth::AuthMethod::Oidc, mfa: true };
        let list = crate::handlers::list_portfolios(State(st), Extension(ctx)).await.unwrap().0;
        assert_eq!(list.len(), 1, "one row per portfolio");
        assert!(list[0].can_trade);
    }

    /// A strategy-owned group, as `reported::strategy::create` makes it:
    /// (group, owner, portfolio).
    async fn strategy_group(st: &AppState) -> (Uuid, Uuid, Uuid) {
        let pool = st.pool();
        let (owner, pf) = (person(pool).await, portfolio(pool).await);
        let code = format!("S{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO strategy (code, name, portfolio_id, owner_principal_id, mode) VALUES ($1, 's', $2, $3, 'REPORTED')")
            .bind(&code).bind(pf).bind(owner).execute(pool).await.unwrap();
        let g = Uuid::new_v4();
        sqlx::query("INSERT INTO user_group (id, name, strategy_code) VALUES ($1, $2, $3)").bind(g).bind(format!("strategy {code}")).bind(&code).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)").bind(g).bind(owner).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, 'manage')").bind(g).bind(pf).execute(pool).await.unwrap();
        (g, owner, pf)
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_adds_person_to_strategy_group_grants_access() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (g, owner, pf) = strategy_group(&st).await;
        let other = person(&pool).await;
        assert!(!allowed(&pool, other, Permission::PositionsView, pf).await.unwrap());
        set_members(State(st.clone()), Extension(admin()), Path(g), Json(vec![owner, other])).await.unwrap();
        assert!(allowed(&pool, other, Permission::PositionsView, pf).await.unwrap());
        let listed = list_groups(State(st.clone())).await.unwrap().0;
        let row = listed.iter().find(|x| x.id == g).unwrap();
        assert!(row.strategy_code.is_some(), "the list says which strategy owns the group");
        set_members(State(st.clone()), Extension(admin()), Path(g), Json(vec![owner])).await.unwrap();
        assert!(!allowed(&pool, other, Permission::PositionsView, pf).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn owner_cannot_be_removed_from_strategy_group() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (g, owner, _) = strategy_group(&st).await;
        let other = person(&pool).await;
        for members in [vec![], vec![other]] {
            let err = set_members(State(st.clone()), Extension(admin()), Path(g), Json(members)).await.unwrap_err();
            assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(err.message, "owner must stay in strategy group");
        }
        assert_eq!(members_of(&mut pool.begin().await.unwrap(), g).await.unwrap(), vec![owner]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn archived_strategy_group_can_be_deleted() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (g, _, _) = strategy_group(&st).await;
        let row = list_groups(State(st.clone())).await.unwrap().0.into_iter().find(|x| x.id == g).unwrap();
        assert!(!row.strategy_archived);
        sqlx::query("UPDATE strategy SET archived_at = now() WHERE code = (SELECT strategy_code FROM user_group WHERE id = $1)")
            .bind(g).execute(&pool).await.unwrap();
        let row = list_groups(State(st.clone())).await.unwrap().0.into_iter().find(|x| x.id == g).unwrap();
        assert!(row.strategy_archived, "the list says the owning strategy is archived");
        delete_group(State(st.clone()), Extension(admin()), Path(g)).await.unwrap();
        let gone: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM user_group WHERE id = $1").bind(g).fetch_optional(&pool).await.unwrap();
        assert!(gone.is_none());
        let (members, grants): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM user_group_member WHERE group_id = $1), \
                    (SELECT count(*) FROM user_group_portfolio WHERE group_id = $1)",
        )
        .bind(g).fetch_one(&pool).await.unwrap();
        assert_eq!((members, grants), (0, 0), "memberships and portfolio grants go with the group");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn strategy_group_cannot_be_deleted() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (g, _, pf) = strategy_group(&st).await;
        let err = delete_group(State(st.clone()), Extension(admin()), Path(g)).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let err = set_portfolios(State(st.clone()), Extension(admin()), Path(g), Json(vec![])).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        let other = portfolio(&pool).await;
        let swapped = vec![GroupPortfolio { portfolio_id: other, code: String::new(), access: Access::View }];
        assert_eq!(set_portfolios(State(st.clone()), Extension(admin()), Path(g), Json(swapped)).await.unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);
        // Keeping the strategy portfolio (and adding another) is fine.
        let both = vec![
            GroupPortfolio { portfolio_id: pf, code: String::new(), access: Access::Manage },
            GroupPortfolio { portfolio_id: other, code: String::new(), access: Access::View },
        ];
        set_portfolios(State(st.clone()), Extension(admin()), Path(g), Json(both)).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM user_group WHERE id = $1").bind(g).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn set_user_groups_keeps_owner_in_strategy_group() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (g, owner, _) = strategy_group(&st).await;
        let plain = group(&st, "plain", &[], &[]).await;
        let mut tx = pool.begin().await.unwrap();
        let err = set_user_groups(&mut tx, owner, &[plain]).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.message, "owner must stay in strategy group");
        tx.rollback().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        set_user_groups(&mut tx, owner, &[plain, g]).await.unwrap();
        tx.commit().await.unwrap();
        // A non-owner may be dropped from a strategy group.
        let other = person(&pool).await;
        let mut tx = pool.begin().await.unwrap();
        set_user_groups(&mut tx, other, &[g]).await.unwrap();
        set_user_groups(&mut tx, other, &[]).await.unwrap();
        tx.commit().await.unwrap();
    }
}
