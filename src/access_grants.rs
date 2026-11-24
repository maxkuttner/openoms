//! Access grants: switches for what a person may use. A person's effective
//! grants are the default grants plus those of every group they belong to;
//! admins hold every grant. Enforcement lives in `authz`.

use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::{Extension, Json};
use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantKey {
    Orders,
    CancelAny,
    Allocate,
    Options,
    Strategies,
    StrategyTokens,
    LimitsView,
    LimitsSet,
    ApiKeys,
}

impl GrantKey {
    pub const ALL: [GrantKey; 9] = [
        GrantKey::Orders,
        GrantKey::CancelAny,
        GrantKey::Allocate,
        GrantKey::Options,
        GrantKey::Strategies,
        GrantKey::StrategyTokens,
        GrantKey::LimitsView,
        GrantKey::LimitsSet,
        GrantKey::ApiKeys,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            GrantKey::Orders => "orders",
            GrantKey::CancelAny => "cancel_any",
            GrantKey::Allocate => "allocate",
            GrantKey::Options => "options",
            GrantKey::Strategies => "strategies",
            GrantKey::StrategyTokens => "strategy_tokens",
            GrantKey::LimitsView => "limits_view",
            GrantKey::LimitsSet => "limits_set",
            GrantKey::ApiKeys => "api_keys",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            GrantKey::Orders => "Place and cancel own orders",
            GrantKey::CancelAny => "Cancel any order",
            GrantKey::Allocate => "Allocate block orders",
            GrantKey::Options => "Trade options and combos",
            GrantKey::Strategies => "Create strategies",
            GrantKey::StrategyTokens => "Rotate and revoke strategy tokens",
            GrantKey::LimitsView => "See risk limits",
            GrantKey::LimitsSet => "Set risk limits",
            GrantKey::ApiKeys => "Personal API keys",
        }
    }
}

fn parse_all(keys: Vec<String>) -> BTreeSet<GrantKey> {
    // A key we no longer know is ignored rather than failing the lookup.
    keys.iter().filter_map(|k| GrantKey::parse(k)).collect()
}

/// Grants every user gets.
pub async fn default_grants<'e>(conn: impl PgExecutor<'e>) -> sqlx::Result<BTreeSet<GrantKey>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT grant_key FROM access_grant WHERE subject_type = 'default'")
        .fetch_all(conn)
        .await?;
    Ok(parse_all(keys))
}

/// Grants a group adds for its members.
pub async fn group_grants<'e>(conn: impl PgExecutor<'e>, group: Uuid) -> sqlx::Result<BTreeSet<GrantKey>> {
    let keys: Vec<String> = sqlx::query_scalar("SELECT grant_key FROM access_grant WHERE subject_type = 'group' AND subject_id = $1")
        .bind(group)
        .fetch_all(conn)
        .await?;
    Ok(parse_all(keys))
}

/// Effective grants: default ∪ the person's groups; admins get every key.
pub async fn grants_for(pool: &PgPool, principal: Uuid) -> sqlx::Result<BTreeSet<GrantKey>> {
    if crate::admin_bootstrap::is_active_admin(pool, principal).await? {
        return Ok(GrantKey::ALL.into_iter().collect());
    }
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT grant_key FROM access_grant WHERE subject_type = 'default' \
         UNION \
         SELECT a.grant_key FROM access_grant a \
           JOIN user_group_member m ON m.group_id = a.subject_id \
          WHERE a.subject_type = 'group' AND m.principal_id = $1",
    )
    .bind(principal)
    .fetch_all(pool)
    .await?;
    Ok(parse_all(keys))
}

async fn replace(tx: &mut Transaction<'_, Postgres>, subject_type: &str, subject: Option<Uuid>, keys: &[GrantKey]) -> Result<(), ApiError> {
    let internal = |e: sqlx::Error| crate::handlers::internal("database error", e);
    sqlx::query("DELETE FROM access_grant WHERE subject_type = $1 AND subject_id IS NOT DISTINCT FROM $2")
        .bind(subject_type)
        .bind(subject)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    let names: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    sqlx::query(
        "INSERT INTO access_grant (subject_type, subject_id, grant_key) SELECT $1, $2, k FROM unnest($3::text[]) AS k ON CONFLICT DO NOTHING",
    )
    .bind(subject_type)
    .bind(subject)
    .bind(&names)
    .execute(&mut **tx)
    .await
    .map_err(internal)?;
    Ok(())
}

/// Replace the default grant set. Audited as `grants.default_set`.
pub async fn set_default_grants(tx: &mut Transaction<'_, Postgres>, ctx: &AuthContext, keys: &[GrantKey]) -> Result<(), ApiError> {
    replace(tx, "default", None, keys).await?;
    let names: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    sqlx::query("INSERT INTO audit_event (actor, action, detail) VALUES ($1, 'grants.default_set', $2)")
        .bind(&ctx.principal_code)
        .bind(serde_json::json!({ "grants": names }))
        .execute(&mut **tx)
        .await
        .map_err(|e| crate::handlers::internal("database error", e))?;
    Ok(())
}

/// Replace a group's grant set. Audited as `grants.group_set`.
pub async fn set_group_grants(tx: &mut Transaction<'_, Postgres>, ctx: &AuthContext, group: Uuid, keys: &[GrantKey]) -> Result<(), ApiError> {
    replace(tx, "group", Some(group), keys).await?;
    let names: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
    crate::groups_api::audit(tx, ctx, "grants.group_set", group, serde_json::json!({ "grants": names })).await
}

// ---- admin API -----------------------------------------------------------

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GrantsBody {
    pub grants: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct CatalogEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub category: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub struct PreviewEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub granted: bool,
    /// Where it comes from: "default" (everyone has it) or "group".
    pub from: &'static str,
}

fn category(k: GrantKey) -> &'static str {
    match k {
        GrantKey::Orders | GrantKey::CancelAny | GrantKey::Allocate | GrantKey::Options => "Trading",
        GrantKey::Strategies | GrantKey::StrategyTokens => "Strategies",
        GrantKey::LimitsView | GrantKey::LimitsSet => "Risk",
        GrantKey::ApiKeys => "Account",
    }
}

fn api_error(status: axum::http::StatusCode, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn internal(e: sqlx::Error) -> ApiError {
    crate::handlers::internal("database error", e)
}

fn names(set: &BTreeSet<GrantKey>) -> Vec<String> {
    set.iter().map(|k| k.as_str().to_string()).collect()
}

/// Validate every key before anything is written.
fn parse_keys(body: &GrantsBody) -> Result<Vec<GrantKey>, ApiError> {
    let mut out = BTreeSet::new();
    for k in &body.grants {
        let key = GrantKey::parse(k).ok_or_else(|| api_error(axum::http::StatusCode::UNPROCESSABLE_ENTITY, format!("unknown grant {k}")))?;
        out.insert(key);
    }
    Ok(out.into_iter().collect())
}

/// `GET /admin/grants/catalog`: every key with label and category, in display order.
pub async fn catalog() -> Json<Vec<CatalogEntry>> {
    Json(GrantKey::ALL.into_iter().map(|k| CatalogEntry { key: k.as_str(), label: k.label(), category: category(k) }).collect())
}

/// `GET /admin/grants/default`
pub async fn get_default(State(state): State<AppState>) -> Result<Json<GrantsBody>, ApiError> {
    let set = default_grants(state.pool()).await.map_err(internal)?;
    Ok(Json(GrantsBody { grants: names(&set) }))
}

/// `PUT /admin/grants/default`: exactly these grants for everyone.
pub async fn put_default(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<GrantsBody>,
) -> Result<Json<GrantsBody>, ApiError> {
    let keys = parse_keys(&body)?;
    let mut tx = state.pool().begin().await.map_err(internal)?;
    set_default_grants(&mut tx, &ctx, &keys).await?;
    tx.commit().await.map_err(internal)?;
    // Everyone is affected: refresh every active person's open sessions.
    let people: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM principal WHERE principal_type = 'HUMAN' AND status = 'ACTIVE'")
        .fetch_all(state.pool())
        .await
        .map_err(internal)?;
    crate::groups_api::announce(&state, people);
    Ok(Json(GrantsBody { grants: names(&keys.into_iter().collect()) }))
}

async fn require_group(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    let found: Option<Uuid> = sqlx::query_scalar("SELECT id FROM user_group WHERE id = $1").bind(id).fetch_optional(state.pool()).await.map_err(internal)?;
    found.map(|_| ()).ok_or_else(|| api_error(axum::http::StatusCode::NOT_FOUND, "group not found"))
}

/// `GET /admin/groups/:id/grants`
pub async fn get_group(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<GrantsBody>, ApiError> {
    require_group(&state, id).await?;
    let set = group_grants(state.pool(), id).await.map_err(internal)?;
    Ok(Json(GrantsBody { grants: names(&set) }))
}

/// `PUT /admin/groups/:id/grants`: exactly these grants for the group's members.
pub async fn put_group(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<GrantsBody>,
) -> Result<Json<GrantsBody>, ApiError> {
    let keys = parse_keys(&body)?;
    require_group(&state, id).await?;
    let mut tx = state.pool().begin().await.map_err(internal)?;
    set_group_grants(&mut tx, &ctx, id, &keys).await?;
    let members: Vec<Uuid> = sqlx::query_scalar("SELECT principal_id FROM user_group_member WHERE group_id = $1")
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    crate::groups_api::announce(&state, members);
    Ok(Json(GrantsBody { grants: names(&keys.into_iter().collect()) }))
}

/// `GET /admin/groups/:id/preview`: what a member of this group would get.
pub async fn preview(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<serde_json::Value>, ApiError> {
    require_group(&state, id).await?;
    let default = default_grants(state.pool()).await.map_err(internal)?;
    let group = group_grants(state.pool(), id).await.map_err(internal)?;
    let grants: Vec<PreviewEntry> = GrantKey::ALL
        .into_iter()
        .map(|k| PreviewEntry {
            key: k.as_str(),
            label: k.label(),
            granted: default.contains(&k) || group.contains(&k),
            from: if default.contains(&k) { "default" } else { "group" },
        })
        .collect();
    Ok(Json(serde_json::json!({ "grants": grants })))
}

/// Test fixture: give `principal` `keys` through a fresh one-member group (as
/// an admin would), for tests whose setup predates access grants.
#[cfg(test)]
pub(crate) async fn grant_via_group(pool: &PgPool, principal: Uuid, keys: &[GrantKey]) {
    let group = Uuid::new_v4();
    sqlx::query("INSERT INTO user_group (id, name) VALUES ($1, $2)")
        .bind(group)
        .bind(format!("grants-{}", group.simple()))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)")
        .bind(group)
        .bind(principal)
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    set_group_grants(&mut tx, &AuthContext::machine(Uuid::nil(), "test".into()), group, keys).await.unwrap();
    tx.commit().await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;
    use crate::groups_api::tests::{admin, person};

    #[test]
    fn keys_round_trip() {
        for k in GrantKey::ALL {
            assert_eq!(GrantKey::parse(k.as_str()), Some(k));
            assert!(!k.label().is_empty());
        }
        assert_eq!(GrantKey::parse("nope"), None);
        assert_eq!(GrantKey::ALL.len(), 9);
    }

    async fn group(pool: &PgPool, members: &[Uuid]) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO user_group (id, name) VALUES ($1, $2)").bind(id).bind(format!("g-{}", id.simple())).execute(pool).await.unwrap();
        for m in members {
            sqlx::query("INSERT INTO user_group_member (group_id, principal_id) VALUES ($1, $2)").bind(id).bind(m).execute(pool).await.unwrap();
        }
        id
    }

    async fn set_group(pool: &PgPool, g: Uuid, keys: &[GrantKey]) {
        let mut tx = pool.begin().await.unwrap();
        set_group_grants(&mut tx, &admin(), g, keys).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[test]
    fn catalog_lists_all_keys_in_order() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let Json(c) = rt.block_on(catalog());
        let got: Vec<_> = c.iter().map(|e| (e.key, e.category)).collect();
        assert_eq!(
            got,
            vec![
                ("orders", "Trading"), ("cancel_any", "Trading"), ("allocate", "Trading"), ("options", "Trading"),
                ("strategies", "Strategies"), ("strategy_tokens", "Strategies"),
                ("limits_view", "Risk"), ("limits_set", "Risk"), ("api_keys", "Account"),
            ]
        );
        assert_eq!(c[0].label, "Place and cancel own orders");
    }

    fn body(keys: &[&str]) -> Json<GrantsBody> {
        Json(GrantsBody { grants: keys.iter().map(|s| s.to_string()).collect() })
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn put_default_grants_round_trips() {
        let pool = migrated_db().await;
        let st = crate::groups_api::tests::state(pool.clone());
        let Json(put) = put_default(State(st.clone()), Extension(admin()), body(&["options", "orders"])).await.unwrap();
        assert_eq!(put.grants, vec!["orders", "options"]);
        let Json(got) = get_default(State(st.clone())).await.unwrap();
        assert_eq!(got.grants, vec!["orders", "options"]);
        let _ = put_default(State(st.clone()), Extension(admin()), body(&[])).await.unwrap();
        assert!(get_default(State(st)).await.unwrap().0.grants.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn unknown_grant_is_422() {
        let pool = migrated_db().await;
        let st = crate::groups_api::tests::state(pool.clone());
        let before = default_grants(&pool).await.unwrap();
        let err = put_default(State(st.clone()), Extension(admin()), body(&["orders", "fly"])).await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(err.message, "unknown grant fly");
        assert_eq!(default_grants(&pool).await.unwrap(), before, "nothing written");
        let g = group(&pool, &[]).await;
        let err = put_group(State(st.clone()), Extension(admin()), Path(g), body(&["fly"])).await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(group_grants(&pool, g).await.unwrap().is_empty());
        let err = put_group(State(st), Extension(admin()), Path(Uuid::new_v4()), body(&["orders"])).await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn preview_marks_default_vs_group() {
        let pool = migrated_db().await;
        let st = crate::groups_api::tests::state(pool.clone());
        let _ = put_default(State(st.clone()), Extension(admin()), body(&["orders"])).await.unwrap();
        let g = group(&pool, &[]).await;
        let _ = put_group(State(st.clone()), Extension(admin()), Path(g), body(&["orders", "options"])).await.unwrap();
        let Json(v) = preview(State(st), Path(g)).await.unwrap();
        let rows = v["grants"].as_array().unwrap();
        assert_eq!(rows.len(), 9);
        let row = |k: &str| rows.iter().find(|r| r["key"] == k).unwrap().clone();
        assert_eq!((row("orders")["granted"].clone(), row("orders")["from"].clone()), (true.into(), "default".into()));
        assert_eq!((row("options")["granted"].clone(), row("options")["from"].clone()), (true.into(), "group".into()));
        assert_eq!(row("limits_set")["granted"], false);
        assert_eq!(row("orders")["label"], "Place and cancel own orders");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn grants_are_a_union() {
        let pool = migrated_db().await;
        let me = person(&pool).await;
        let (g1, g2) = (group(&pool, &[me]).await, group(&pool, &[me]).await);
        let mut tx = pool.begin().await.unwrap();
        set_default_grants(&mut tx, &admin(), &[GrantKey::Orders]).await.unwrap();
        tx.commit().await.unwrap();
        set_group(&pool, g1, &[GrantKey::CancelAny]).await;
        set_group(&pool, g2, &[GrantKey::CancelAny, GrantKey::Options]).await;
        let got = grants_for(&pool, me).await.unwrap();
        assert_eq!(got, BTreeSet::from([GrantKey::Orders, GrantKey::CancelAny, GrantKey::Options]));
        assert_eq!(default_grants(&pool).await.unwrap(), BTreeSet::from([GrantKey::Orders]));
        // replace, not append
        set_group(&pool, g2, &[]).await;
        assert_eq!(group_grants(&pool, g2).await.unwrap(), BTreeSet::new());
        assert_eq!(grants_for(&pool, me).await.unwrap(), BTreeSet::from([GrantKey::Orders, GrantKey::CancelAny]));
        let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action IN ('grants.default_set','grants.group_set')")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(audited, 4);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn group_delete_drops_its_grants() {
        let pool = migrated_db().await;
        let me = person(&pool).await;
        let g = group(&pool, &[me]).await;
        set_group(&pool, g, &[GrantKey::LimitsSet]).await;
        sqlx::query("DELETE FROM user_group WHERE id = $1").bind(g).execute(&pool).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM access_grant WHERE subject_id = $1").bind(g).fetch_one(&pool).await.unwrap();
        assert_eq!(left, 0);
        assert!(!grants_for(&pool, me).await.unwrap().contains(&GrantKey::LimitsSet));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_has_every_grant() {
        let pool = migrated_db().await;
        let me = person(&pool).await;
        sqlx::query("DELETE FROM access_grant").execute(&pool).await.unwrap();
        assert!(grants_for(&pool, me).await.unwrap().is_empty());
        sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, $1)").bind(me).execute(&pool).await.unwrap();
        assert_eq!(grants_for(&pool, me).await.unwrap().len(), GrantKey::ALL.len());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn migration_preserves_current_access() {
        let pool = migrated_db().await;
        let (pm, plain) = (person(&pool).await, person(&pool).await);
        let (managers, others) = (group(&pool, &[pm]).await, group(&pool, &[]).await);
        let pf = crate::groups_api::tests::portfolio(&pool).await;
        for (g, access) in [(managers, "manage"), (others, "trade")] {
            sqlx::query("INSERT INTO user_group_portfolio (group_id, portfolio_id, access) VALUES ($1, $2, $3)")
                .bind(g).bind(pf).bind(access).execute(&pool).await.unwrap();
        }
        // Re-run the migration's seed statements on this fixture.
        sqlx::query("DELETE FROM access_grant").execute(&pool).await.unwrap();
        let sql = include_str!("../db/migrations/ods/oms/0075_ACCESS_GRANTS.sql");
        let seed = sql.split("-- seed").nth(1).and_then(|s| s.split_once('\n')).expect("seed marker").1;
        sqlx::raw_sql(seed).execute(&pool).await.unwrap();

        let pm_grants = grants_for(&pool, pm).await.unwrap();
        assert!(pm_grants.contains(&GrantKey::CancelAny) && pm_grants.contains(&GrantKey::LimitsSet));
        let plain_grants = grants_for(&pool, plain).await.unwrap();
        assert!(plain_grants.contains(&GrantKey::Orders) && plain_grants.contains(&GrantKey::LimitsView));
        assert!(!plain_grants.contains(&GrantKey::CancelAny) && !plain_grants.contains(&GrantKey::LimitsSet));
        assert!(group_grants(&pool, others).await.unwrap().is_empty());
    }
}
