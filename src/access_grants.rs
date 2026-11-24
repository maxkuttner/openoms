//! Access grants: switches for what a person may use. A person's effective
//! grants are the default grants plus those of every group they belong to;
//! admins hold every grant. Enforcement lives in `authz`.

use std::collections::BTreeSet;

use sqlx::{PgExecutor, PgPool, Postgres, Transaction};
use uuid::Uuid;

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
