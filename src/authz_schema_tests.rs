//! DB tests for the users/roles/SSO schema (migrations 0047–0052): the
//! backfill of existing principals into identities and roles.

use sqlx::postgres::{PgConnectOptions, PgPool};
use uuid::Uuid;

use crate::setup::database::migrate;

const FIRST_AUTHZ_MIGRATION: &str = "0047_";

/// A fresh, empty database on the disposable server, so a migration can be
/// stopped short of 0047 regardless of what the shared `ods` database holds.
pub(crate) async fn fresh_db() -> PgPool {
    let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL")
        .expect("set OMS_RELOAD_TEST_DATABASE_URL to an explicit disposable Postgres server");
    let options: PgConnectOptions = url.parse().unwrap();
    let admin = PgPool::connect_with(options.clone()).await.unwrap();
    let name = format!("oms_authz_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name} OWNER oms"))
        .execute(&admin)
        .await
        .unwrap();
    PgPool::connect_with(options.database(&name)).await.unwrap()
}

/// A fully migrated disposable database whose connections resolve
/// unqualified table names in `oms`, as the runtime role does.
pub(crate) async fn migrated_db() -> PgPool {
    let admin = fresh_db().await;
    migrate::ensure_tracking(&admin).await.unwrap();
    migrate::apply_all(&admin).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("ALTER DATABASE {name} SET search_path = oms, public"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let url = std::env::var("OMS_RELOAD_TEST_DATABASE_URL").unwrap();
    let options: PgConnectOptions = url.parse().unwrap();
    PgPool::connect_with(options.database(&name)).await.unwrap()
}

async fn principal(pool: &PgPool, code: &str, kind: &str, subject: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO oms.principal (id, code, principal_type, external_subject, status) \
         VALUES ($1, $2, $3, $4, 'ACTIVE')",
    )
    .bind(id)
    .bind(code)
    .bind(kind)
    .bind(subject)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn role_of(pool: &PgPool, id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT role_code FROM oms.principal_role WHERE principal_id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn migrations_backfill_identities_and_roles() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_matching(&pool, |schema, file| {
        schema != "oms" || file < FIRST_AUTHZ_MIGRATION
    })
    .await
    .unwrap();

    let human = principal(&pool, "alice", "HUMAN", Some("sub-1")).await;
    let service = principal(&pool, "svc", "SERVICE", None).await;
    sqlx::query(
        "INSERT INTO oms.api_key (principal_id, key_id, secret_hash) VALUES ($1, 'k-svc', 'h')",
    )
    .bind(service)
    .execute(&pool)
    .await
    .unwrap();
    let desk = principal(&pool, "desk", "DESK", None).await;
    let strategy = principal(&pool, "algo", "STRATEGY", None).await;

    std::env::set_var(migrate::BACKFILL_ISSUER_ENV, "https://idp.test");
    migrate::apply_all(&pool).await.unwrap();

    let linked: Option<Uuid> = sqlx::query_scalar(
        "SELECT principal_id FROM oms.principal_identity WHERE issuer = $1 AND subject = $2",
    )
    .bind("https://idp.test")
    .bind("sub-1")
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(linked, Some(human));

    let column: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_schema = 'oms' AND table_name = 'principal' AND column_name = 'external_subject'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(column, 0, "external_subject must be dropped");

    assert_eq!(role_of(&pool, human).await.as_deref(), Some("trader"));
    assert_eq!(role_of(&pool, strategy).await.as_deref(), Some("strategy"));

    let service_type: String =
        sqlx::query_scalar("SELECT principal_type FROM oms.principal WHERE id = $1")
            .bind(service)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(service_type, "HUMAN");
    assert_eq!(role_of(&pool, service).await.as_deref(), Some("trader"));

    let desk_left: i64 = sqlx::query_scalar("SELECT count(*) FROM oms.principal WHERE id = $1")
        .bind(desk)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(desk_left, 0, "unused DESK principal must be deleted");
}

/// A strategy's responsible user must be a person, never another strategy.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn strategy_owner_cannot_be_a_strategy_principal() {
    let pool = fresh_db().await;
    migrate::ensure_tracking(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let portfolio = Uuid::new_v4();
    sqlx::query("INSERT INTO oms.portfolio (id, code, name, status) VALUES ($1, 'p-authz', 'p', 'ACTIVE')")
        .bind(portfolio)
        .execute(&pool)
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    let algo = Uuid::new_v4();
    for (id, code) in [(owner, "algo-owner"), (algo, "algo-self")] {
        sqlx::query(
            "INSERT INTO oms.principal (id, code, principal_type, status) \
             VALUES ($1, $2, 'STRATEGY', 'ACTIVE')",
        )
        .bind(id)
        .bind(code)
        .execute(&pool)
        .await
        .unwrap();
    }
    let err = sqlx::query(
        "INSERT INTO oms.strategy (code, name, portfolio_id, owner_principal_id, principal_id, \
         sender_comp_id, target_comp_id) VALUES ('s', 's', $1, $2, $3, 'A', 'B')",
    )
    .bind(portfolio)
    .bind(owner)
    .bind(algo)
    .execute(&pool)
    .await
    .expect_err("a STRATEGY owner must be refused");
    assert!(err.to_string().contains("STRATEGY"), "{err}");
}
