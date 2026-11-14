//! DB tests for the users/roles/SSO schema (migrations 0047–0059): the
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

    assert_eq!(role_of(&pool, human).await.as_deref(), Some("pm"));
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

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn auth_redesign_schema() {
    let pool = migrated_db().await;

    for (table, column) in [
        ("user_session", "auth_method"),
        ("user_session", "mfa_verified_at"),
        ("principal_admin", "principal_id"),
        ("principal_admin", "granted_by"),
        ("principal_admin", "granted_at"),
        ("local_credential", "password_hash"),
        ("local_credential", "password_changed_at"),
        ("local_credential", "totp_secret_enc"),
        ("local_credential", "totp_enrolled_at"),
        ("local_credential", "totp_last_step"),
        ("local_credential", "failed_attempts"),
        ("local_credential", "locked_until"),
        ("recovery_code", "code_hash"),
        ("recovery_code", "used_at"),
        ("pending_identity_binding", "invite_id"),
        ("pending_identity_binding", "issuer"),
        ("pending_identity_binding", "subject"),
        ("pending_identity_binding", "untrusted_email"),
        ("pending_identity_binding", "display_name"),
        ("pending_identity_binding", "metadata"),
        ("pending_identity_binding", "expires_at"),
        ("pending_identity_binding", "decided_at"),
        ("pending_identity_binding", "decided_by"),
        ("pending_identity_binding", "decision"),
        ("invite", "method"),
        ("system_setting", "key"),
        ("system_setting", "value"),
        ("system_setting", "updated_at"),
    ] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'oms' AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "missing column {table}.{column}");
    }

    let legacy: serde_json::Value = sqlx::query_scalar(
        "SELECT value FROM oms.system_setting WHERE key = 'legacy_admin_enabled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(legacy, serde_json::json!(true));

    // A STRATEGY principal cannot be a named admin; a HUMAN can.
    let mut ids = Vec::new();
    for (code, kind) in [("auth_strat", "STRATEGY"), ("auth_human", "HUMAN")] {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO oms.principal (id, code, principal_type, status) \
             VALUES ($1, $2, $3, 'ACTIVE')",
        )
        .bind(id)
        .bind(code)
        .bind(kind)
        .execute(&pool)
        .await
        .unwrap();
        ids.push(id);
    }
    let (strategy, human) = (ids[0], ids[1]);
    let err = sqlx::query(
        "INSERT INTO oms.principal_admin (principal_id, granted_by) VALUES ($1, 'test')",
    )
    .bind(strategy)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("STRATEGY"), "{err}");
    sqlx::query("INSERT INTO oms.principal_admin (principal_id, granted_by) VALUES ($1, 'test')")
        .bind(human)
        .execute(&pool)
        .await
        .unwrap();

    // One open binding per (issuer, subject); a decided one frees the slot.
    let invite = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO oms.invite (id, token_hash, email, principal_id, created_by, expires_at) \
         VALUES ($1, 'auth_tok', 'a@example.com', $2, 'test', now() + interval '1 day')",
    )
    .bind(invite)
    .bind(human)
    .execute(&pool)
    .await
    .unwrap();
    let method: String = sqlx::query_scalar("SELECT method FROM oms.invite WHERE id = $1")
        .bind(invite)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(method, "any");
    let open = "INSERT INTO oms.pending_identity_binding \
                (id, invite_id, issuer, subject, expires_at) \
                VALUES ($1, $2, 'https://idp', 'sub-1', now() + interval '1 day')";
    sqlx::query(open)
        .bind(Uuid::new_v4())
        .bind(invite)
        .execute(&pool)
        .await
        .unwrap();
    let dup = sqlx::query(open)
        .bind(Uuid::new_v4())
        .bind(invite)
        .execute(&pool)
        .await;
    assert!(
        dup.is_err(),
        "second open binding for the same identity must fail"
    );
    sqlx::query(
        "UPDATE oms.pending_identity_binding SET decision = 'rejected', \
         decided_at = now(), decided_by = 'test'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(open)
        .bind(Uuid::new_v4())
        .bind(invite)
        .execute(&pool)
        .await
        .unwrap();

    // Sessions default to the OIDC method.
    let session: Uuid = sqlx::query_scalar(
        "INSERT INTO oms.user_session (principal_id, token_hash, absolute_expires_at) \
         VALUES ($1, 'auth_sess', now() + interval '1 day') RETURNING id",
    )
    .bind(human)
    .fetch_one(&pool)
    .await
    .unwrap();
    let auth_method: String =
        sqlx::query_scalar("SELECT auth_method FROM oms.user_session WHERE id = $1")
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(auth_method, "oidc");
}
