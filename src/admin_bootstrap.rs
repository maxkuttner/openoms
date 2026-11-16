//! Named admins: first-admin bootstrap from the host and break-glass recovery.
//!
//! Admin is a global capability (a `principal_admin` row), separate from roles
//! and portfolio grants: an admin can run the console and cannot trade until
//! someone gives them a role and grants.
//!
//! On a fresh install the first account signed up becomes the admin (see
//! `local_auth_api::signup`).

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::auth::AuthContext;

/// Audit actor of the host CLI (`oms auth …`, `oms init`).
pub const HOST_CLI_ACTOR: &str = "host-cli";

/// Does `principal_id` hold the admin capability and is it ACTIVE?
pub async fn is_active_admin(pool: &PgPool, principal_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal_admin a JOIN principal p ON p.id = a.principal_id \
                         WHERE a.principal_id = $1 AND p.status = 'ACTIVE')",
    )
    .bind(principal_id)
    .fetch_one(pool)
    .await
}

/// Serialise every change to who is an admin (grant, revoke, disable,
/// bootstrap), so two concurrent removals cannot both see "one other admin
/// left". Held until the transaction ends.
pub async fn lock_admins(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("LOCK TABLE principal_admin IN SHARE ROW EXCLUSIVE MODE").execute(&mut **tx).await?;
    Ok(())
}

/// ACTIVE admins other than `except`. Call after `lock_admins`.
pub async fn other_active_admins(tx: &mut Transaction<'_, Postgres>, except: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM principal_admin a JOIN principal p ON p.id = a.principal_id \
          WHERE p.status = 'ACTIVE' AND a.principal_id <> $1",
    )
    .bind(except)
    .fetch_one(&mut **tx)
    .await
}

#[derive(Debug)]
pub enum BootstrapError {
    /// An ACTIVE admin exists and recovery was not forced.
    AdminExists,
    /// No admin (not disabled) has this email.
    NotAnAdmin,
    Account(crate::accounts::AccountError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for BootstrapError {
    fn from(err: sqlx::Error) -> Self {
        BootstrapError::Db(err)
    }
}

impl From<crate::accounts::AccountError> for BootstrapError {
    fn from(err: crate::accounts::AccountError) -> Self {
        match err {
            crate::accounts::AccountError::Db(e) => BootstrapError::Db(e),
            other => BootstrapError::Account(other),
        }
    }
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::AdminExists => f.write_str(
                "an active admin already exists; sign in as them, or use \
                 `oms auth recover-admin --email …` (or --force-recovery) if nobody can",
            ),
            BootstrapError::NotAnAdmin => f.write_str("no admin with this email"),
            BootstrapError::Account(e) => write!(f, "{e}"),
            BootstrapError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

/// A password the host CLI generated is someone else's choice: it must be
/// changed at first sign-in, and dies after 24 h if never used.
async fn mark_cli_password(tx: &mut Transaction<'_, Postgres>, principal_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE local_credential SET must_change_password = true, password_expires_at = now() + interval '24 hours' \
          WHERE principal_id = $1",
    )
    .bind(principal_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A one-time password the host CLI prints; it must be changed at sign-in.
fn one_time_password() -> String {
    crate::setup::init::generate_password()
}

/// Create an ACTIVE local admin with a generated password (returned once):
/// the admin capability, no portfolio access. With `only_if_no_admin`,
/// refused while any ACTIVE admin exists. Audited `admin.bootstrap` by `actor`.
pub async fn bootstrap(
    pool: &PgPool,
    email: &str,
    display_name: Option<&str>,
    actor: &str,
    only_if_no_admin: bool,
) -> Result<(Uuid, String), BootstrapError> {
    let email = crate::accounts::normalise_email(email)?;
    let password = one_time_password();
    let hash = crate::local_auth::hash_password(&password)
        .await
        .map_err(|e| BootstrapError::Db(sqlx::Error::Protocol(format!("hash password: {e}"))))?;
    let mut tx = pool.begin().await?;
    lock_admins(&mut tx).await?;
    if only_if_no_admin && other_active_admins(&mut tx, Uuid::nil()).await? > 0 {
        return Err(BootstrapError::AdminExists);
    }
    let acct = crate::accounts::NewAccount {
        name: display_name,
        email: Some(&email),
        password_hash: Some(&hash),
        role: crate::accounts::AccountRole::Admin,
    };
    let (id, _) = crate::accounts::create_account(&mut tx, &acct, actor, "admin.bootstrap").await?;
    mark_cli_password(&mut tx, id).await?;
    tx.commit().await?;
    Ok((id, password))
}

/// Break-glass: give the admin a new generated password (returned once),
/// remove their TOTP and recovery codes, unlock the credential and revoke
/// every session. Audited `admin.recovered`.
///
/// Run from the CLI, a separate process that cannot publish `AuthChanged`:
/// the admin's open sockets close at their next 15 s heartbeat re-check.
pub async fn recover(pool: &PgPool, email: &str, actor: &str) -> Result<(Uuid, String), BootstrapError> {
    let email = crate::accounts::normalise_email(email)?;
    let password = one_time_password();
    let hash = crate::local_auth::hash_password(&password)
        .await
        .map_err(|e| BootstrapError::Db(sqlx::Error::Protocol(format!("hash password: {e}"))))?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT p.id FROM principal p JOIN principal_admin a ON a.principal_id = p.id \
          WHERE lower(p.email) = $1 AND p.principal_type = 'HUMAN' AND p.status <> 'DISABLED' \
          FOR UPDATE OF p",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BootstrapError::NotAnAdmin)?;
    let principal_id: Uuid = row.get("id");
    sqlx::query("DELETE FROM recovery_code WHERE principal_id = $1").bind(principal_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO local_credential (principal_id, password_hash) VALUES ($1, $2) \
         ON CONFLICT (principal_id) DO UPDATE SET password_hash = EXCLUDED.password_hash, \
             totp_secret_enc = NULL, totp_enrolled_at = NULL, totp_last_step = NULL, \
             failed_attempts = 0, locked_until = NULL",
    )
    .bind(principal_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO principal_identity (issuer, subject, principal_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(crate::local_auth_api::LOCAL_ISSUER)
    .bind(principal_id.to_string())
    .bind(principal_id)
    .execute(&mut *tx)
    .await?;
    mark_cli_password(&mut tx, principal_id).await?;
    sqlx::query("UPDATE principal SET status = 'ACTIVE', updated_at = now() WHERE id = $1").bind(principal_id).execute(&mut *tx).await?;
    let sessions = sqlx::query("UPDATE user_session SET revoked_at = now() WHERE principal_id = $1 AND revoked_at IS NULL")
        .bind(principal_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    crate::accounts::audit(&mut tx, actor, "admin.recovered", principal_id, serde_json::json!({ "sessions_revoked": sessions })).await?;
    tx.commit().await?;
    Ok((principal_id, password))
}

// ── host token ─────────────────────────────────────────────────────────────

/// File name of the per-boot host token, next to the config file in effect.
pub const HOST_TOKEN_FILE: &str = "host-token";

/// Where the host token lives: beside the `oms.toml` in effect, else beside
/// the one `oms init` would create. Server and CLI resolve it identically.
/// Under Docker it is inside the container and the host's calls arrive from
/// the bridge, not loopback: run host CLI commands with `docker exec`.
pub fn host_token_path() -> std::path::PathBuf {
    let cfg = crate::config::path();
    let cfg = if cfg.exists() { cfg } else { crate::config::init_target() };
    let cfg = crate::config::absolute(cfg);
    cfg.parent().map(|d| d.join(HOST_TOKEN_FILE)).unwrap_or_else(|| HOST_TOKEN_FILE.into())
}

/// Generate a new host token (32 random bytes, hex), write it to `path` with
/// mode 0600 (replacing the previous boot's), and return its SHA-256 — the
/// only form the server keeps.
pub fn write_host_token(path: &std::path::Path) -> std::io::Result<String> {
    use rand::RngCore;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // `mode` applies only on creation; an older file keeps its bits otherwise.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(token.as_bytes())?;
    Ok(crate::sessions::hash_session_token(&token))
}

/// The host token for a CLI call to the running server on this machine.
pub fn read_host_token() -> Result<String, String> {
    let path = host_token_path();
    std::fs::read_to_string(&path).map(|t| t.trim().to_string()).map_err(|e| {
        format!(
            "cannot read the host token at {} ({e}). {}",
            path.display(),
            HOST_CLI_HINT
        )
    })
}

/// Where a host CLI call has to run from, for every refusal or missing token.
pub const HOST_CLI_HINT: &str = "Run this on the OMS host as the OMS user, with the server running \
     (under Docker: inside the container, `docker exec <container> oms ...`).";

/// The error for a host-token call the server answered `status` to: 401/403
/// means the token was refused (wrong host, stale token, or a Docker bridge
/// peer), so say where to run it.
pub fn host_call_error(status: u16, body: &str) -> String {
    if status == 401 || status == 403 {
        format!("the OMS refused the host token ({status}). {HOST_CLI_HINT}")
    } else {
        format!("the OMS answered {status}: {body}")
    }
}

/// The synthetic context of the host token: actor `host-cli`, the nil UUID
/// (never a `principal`), so it can never pass a trading or authz check.
pub fn host_context() -> AuthContext {
    AuthContext {
        principal_id: Uuid::nil(),
        principal_code: HOST_CLI_ACTOR.to_string(),
        auth_method: crate::auth::AuthMethod::HostCli,
        mfa: false,
    }
}

/// Does `token`, from `peer`, match this boot's host token? Only a loopback
/// peer may present it; with no peer address known, refuse.
pub fn host_token_ok(hash: Option<&str>, token: &str, peer: Option<std::net::SocketAddr>) -> bool {
    let (Some(hash), Some(peer)) = (hash, peer) else { return false };
    if !peer.ip().to_canonical().is_loopback() {
        return false;
    }
    let given = crate::sessions::hash_session_token(token);
    given.len() == hash.len() && given.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

// ── host CLI ───────────────────────────────────────────────────────────────

/// A pool on the database `oms database migrate` would use, resolving
/// unqualified names in `oms`.
pub async fn cli_pool(o: crate::setup::database::config::PostgresOverrides) -> Result<PgPool, sqlx::Error> {
    let cfg = crate::setup::database::config::resolve(o);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect(&cfg.url())
        .await
}

/// What the operator must know before using a printed password: under
/// `policy = "sso_only"` local sign-in is off, so it cannot be used as is.
pub fn cli_password_note(policy: crate::config::AuthPolicy) -> Option<&'static str> {
    (policy == crate::config::AuthPolicy::SsoOnly).then_some(
        "note: [auth].policy is \"sso_only\", so local sign-in is off and this password cannot be\n\
         used as is. Set policy = \"local_and_sso\" in oms.toml and restart, sign in, then set\n\
         policy = \"sso_only\" back and restart.",
    )
}

/// The note for the `oms.toml` in effect, if any.
pub fn cli_password_note_for_config() -> Option<&'static str> {
    crate::config::load().and_then(|f| cli_password_note(f.auth_settings().policy))
}

/// `oms auth bootstrap-admin`. Returns the one-time password.
pub async fn cli_bootstrap(
    o: crate::setup::database::config::PostgresOverrides,
    email: &str,
    name: Option<&str>,
    force_recovery: bool,
) -> Result<String, String> {
    let pool = cli_pool(o).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    let (_, password) = bootstrap(&pool, email, name, HOST_CLI_ACTOR, !force_recovery).await.map_err(|e| e.to_string())?;
    Ok(password)
}

/// `oms auth recover-admin`. Returns the new one-time password.
pub async fn cli_recover(o: crate::setup::database::config::PostgresOverrides, email: &str) -> Result<String, String> {
    let pool = cli_pool(o).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    let (_, password) = recover(&pool, email, HOST_CLI_ACTOR).await.map_err(|e| e.to_string())?;
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::authz_schema_tests::migrated_db;
    use crate::local_auth_api::{self as la_api, CodeBody, LocalAuth, LoginBody};
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::{Extension, Json};

    const ORIGIN: &str = "http://localhost:3001";

    /// Strict MFA (`require_mfa = true`): these tests are about the MFA gate
    /// on admin routes; the optional default is covered in `first_run_tests`.
    fn state(pool: PgPool) -> AppState {
        let mut st = crate::handlers::tests::test_app_state(pool);
        st.session_config.public_base_url = Some(ORIGIN.to_string());
        st.require_mfa = true;
        st
    }

    fn la() -> LocalAuth {
        let key = crate::secrets::parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        LocalAuth::new(Some(key), true)
    }

    enum Cred<'a> {
        None,
        Bearer(&'a str),
        Cookie(&'a str),
    }

    async fn send(st: &AppState, method: &str, path: &str, cred: Cred<'_>, body: Option<serde_json::Value>) -> (StatusCode, String) {
        let app = crate::build_app(st.clone(), None, true, false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}{path}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut req = client.request(method.parse().unwrap(), url).header("origin", ORIGIN);
        match cred {
            Cred::None => {}
            Cred::Bearer(t) => req = req.header("authorization", format!("Bearer {t}")),
            Cred::Cookie(c) => req = req.header("cookie", format!("oms_session={c}")),
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let text = resp.text().await.unwrap();
        server.abort();
        (status, text)
    }

    #[test]
    fn cli_password_note_only_under_sso_only() {
        use crate::config::AuthPolicy;
        assert!(cli_password_note(AuthPolicy::LocalAndSso).is_none());
        assert!(cli_password_note(AuthPolicy::SsoOnly).unwrap().contains("local_and_sso"));
    }

    fn hdrs(cookie: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("origin", ORIGIN.parse().unwrap());
        if let Some(c) = cookie {
            h.insert("cookie", format!("oms_session={c}").parse().unwrap());
        }
        h
    }

    fn cookie_of(resp: &axum::response::Response) -> String {
        let raw = resp.headers().get(axum::http::header::SET_COOKIE).unwrap().to_str().unwrap();
        raw.split(';').next().unwrap().split_once('=').unwrap().1.to_string()
    }

    async fn json(resp: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    fn email() -> String {
        format!("admin-{}@fund.test", Uuid::new_v4().simple())
    }

    async fn login(st: &AppState, email: &str, password: &str) -> axum::response::Response {
        la_api::login(
            State(st.clone()),
            Extension(la()),
            None,
            hdrs(None),
            Json(LoginBody { email: email.into(), password: password.into() }),
        )
        .await
    }

    const NEW_PASSWORD: &str = "the admin's own long passphrase";

    /// A bootstrapped admin signed in, past the forced password change: MFA
    /// still pending (strict mode). Returns (principal_id, cookie).
    async fn signed_in_admin(st: &AppState, email: &str) -> (Uuid, String) {
        let (id, password) = bootstrap(st.pool(), email, Some("Ada"), HOST_CLI_ACTOR, false).await.unwrap();
        let resp = login(st, email, &password).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = la_api::change_password(
            State(st.clone()),
            Extension(la()),
            None,
            hdrs(Some(&cookie_of(&resp))),
            Json(la_api::ChangePasswordBody { current_password: password, new_password: NEW_PASSWORD.into() }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        (id, cookie_of(&resp))
    }

    /// Finish TOTP enrolment on `cookie`; returns the MFA-verified cookie.
    async fn verify_totp(st: &AppState, cookie: &str) -> String {
        let resp = la_api::totp_enroll(State(st.clone()), Extension(la()), hdrs(Some(cookie))).await;
        let uri = json(resp).await["otpauth_uri"].as_str().unwrap().to_string();
        let code = totp_rs::Totp::from_url(&uri).unwrap().generate(chrono::Utc::now().timestamp() as u64).to_string();
        let resp = la_api::totp_confirm(State(st.clone()), Extension(la()), None, hdrs(Some(cookie)), Json(CodeBody { code })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        cookie_of(&resp)
    }

    async fn mfa_admin(st: &AppState, email: &str) -> (Uuid, String) {
        let (id, cookie) = signed_in_admin(st, email).await;
        (id, verify_totp(st, &cookie).await)
    }

    async fn audit_rows(pool: &PgPool, action: &str) -> Vec<(String, Option<Uuid>)> {
        sqlx::query_as("SELECT actor, target_principal_id FROM audit_event WHERE action = $1 ORDER BY id")
            .bind(action)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// A PM with a live OIDC session (OIDC needs no TOTP).
    async fn pm_session(pool: &PgPool) -> String {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, display_name, status) VALUES ($1, $2, 'HUMAN', 'Pm', 'ACTIVE')")
            .bind(id)
            .bind(format!("pm-{}", id.simple()))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'pm')").bind(id).execute(pool).await.unwrap();
        crate::sessions::create_session(pool, id, crate::sessions::AuthMethod::Oidc, true, &Default::default(), None)
            .await
            .unwrap()
            .plaintext
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn host_cli_bootstrap_creates_an_admin_who_cannot_trade() {
        let pool = migrated_db().await;
        let (id, password) = bootstrap(&pool, &email(), Some("Ada"), HOST_CLI_ACTOR, true).await.unwrap();
        assert!(password.len() >= 15);
        let (status, name): (String, Option<String>) =
            sqlx::query_as("SELECT status, display_name FROM principal WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((status.as_str(), name.as_deref()), ("ACTIVE", Some("Ada")));
        let (roles, grants): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM principal_role WHERE principal_id = $1), \
                    (SELECT count(*) FROM principal_portfolio_grant WHERE principal_id = $1)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((roles, grants), (0, 0), "admin is not a trading role");
        assert_eq!(audit_rows(&pool, "admin.bootstrap").await, vec![(HOST_CLI_ACTOR.to_string(), Some(id))]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_routes_need_admin_capability() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let pm = pm_session(&pool).await;
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&pm), None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::None, None).await.0, StatusCode::UNAUTHORIZED);
        let (_, admin) = mfa_admin(&st, &email()).await;
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&admin), None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn local_admin_needs_mfa_for_mutations_in_strict_mode() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let (_, cookie) = signed_in_admin(&st, &email()).await;
        let body = serde_json::json!({ "code": "p1", "name": "P1" });
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&cookie), None).await.0, StatusCode::OK, "reads need no MFA");
        let (status, text) = send(&st, "POST", "/admin/portfolios", Cred::Cookie(&cookie), Some(body.clone())).await;
        assert_eq!((status, text.as_str()), (StatusCode::FORBIDDEN, crate::authz::MFA_REQUIRED_BODY));
        let cookie = verify_totp(&st, &cookie).await;
        assert_ne!(send(&st, "POST", "/admin/portfolios", Cred::Cookie(&cookie), Some(body)).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_does_not_imply_trading() {
        let pool = crate::handlers::tests::test_pool().await;
        let st = state(pool.clone());
        let (id, cookie) = mfa_admin(&st, &email()).await;
        let session = crate::auth::authenticate(&pool, &hdrs(Some(&cookie)), &st.session_config)
            .await
            .ok()
            .flatten()
            .expect("the admin session authenticates");
        assert_eq!(session.0.principal_id, id);
        let (portfolio, instrument) = crate::authz_route_tests::tradeable_portfolio(&pool).await;
        let (_, status) = crate::authz_route_tests::submit(&pool, &session.0, portfolio, instrument).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = send(&st, "GET", "/portfolios", Cred::Cookie(&cookie), None).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"), "no portfolio grants come with admin");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cli_bootstrap_refused_when_admin_exists() {
        let pool = migrated_db().await;
        bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, true).await.unwrap();
        assert!(matches!(bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, true).await, Err(BootstrapError::AdminExists)));
        bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, false).await.unwrap();
        let taken = email();
        bootstrap(&pool, &taken, None, HOST_CLI_ACTOR, false).await.unwrap();
        assert!(matches!(bootstrap(&pool, &taken, None, HOST_CLI_ACTOR, false).await, Err(BootstrapError::Account(_))));
        assert!(matches!(bootstrap(&pool, "nope", None, HOST_CLI_ACTOR, false).await, Err(BootstrapError::Account(_))));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cli_recover_resets_password_and_revokes_sessions() {
        let pool = migrated_db().await;
        let st = state(pool.clone());
        let e = email();
        let (id, cookie) = mfa_admin(&st, &e).await;

        let (rid, password) = recover(&pool, &e.to_uppercase(), HOST_CLI_ACTOR).await.unwrap();
        assert_eq!(rid, id);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&cookie), None).await.0, StatusCode::UNAUTHORIZED);
        let (live, totp, codes): (i64, bool, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM user_session WHERE principal_id = $1 AND revoked_at IS NULL), \
                    (SELECT totp_enrolled_at IS NOT NULL FROM local_credential WHERE principal_id = $1), \
                    (SELECT count(*) FROM recovery_code WHERE principal_id = $1)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((live, totp, codes), (0, false, 0));
        assert_eq!(audit_rows(&pool, "admin.recovered").await, vec![(HOST_CLI_ACTOR.to_string(), Some(id))]);
        assert_eq!(login(&st, &e, &password).await.status(), StatusCode::OK, "the new password works");
        assert_eq!(login(&st, &e, NEW_PASSWORD).await.status(), StatusCode::UNAUTHORIZED, "the old one does not");
        assert!(matches!(recover(&pool, &email(), HOST_CLI_ACTOR).await, Err(BootstrapError::NotAnAdmin)));
    }

    fn temp_token_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("oms-host-token-{}", Uuid::new_v4().simple())).join(HOST_TOKEN_FILE)
    }

    /// A state holding a freshly written host token; returns the token.
    fn with_host_token(st: &mut AppState) -> String {
        let path = temp_token_path();
        st.host_token_hash = Some(write_host_token(&path).unwrap());
        let token = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        token
    }

    #[test]
    fn a_refused_host_token_call_says_docker_exec() {
        for status in [401, 403] {
            let m = host_call_error(status, "unauthorized");
            assert!(m.contains("docker exec") && m.contains("OMS host"), "{m}");
        }
        assert!(host_call_error(500, "boom").contains("boom"));
        assert!(HOST_CLI_HINT.contains("docker exec"));
    }

    #[test]
    fn host_token_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_token_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_host_token(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn host_token_rotates_each_boot() {
        let path = temp_token_path();
        let first_hash = write_host_token(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let second_hash = write_host_token(&path).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(second.len(), 64);
        assert_ne!(first, second);
        assert_ne!(first_hash, second_hash);
        assert!(!second.contains(&second_hash), "only the hash is kept, never the token");
        let lo = Some("127.0.0.1:5000".parse().unwrap());
        assert!(!host_token_ok(Some(&second_hash), &first, lo), "the previous boot's token is dead");
        assert!(host_token_ok(Some(&second_hash), &second, lo));
    }

    #[test]
    fn host_token_refused_from_non_loopback_peer() {
        let path = temp_token_path();
        let hash = write_host_token(&path).unwrap();
        let token = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        for peer in ["10.0.0.1:5000", "192.168.1.2:5000", "[2001:db8::1]:5000"] {
            assert!(!host_token_ok(Some(&hash), &token, Some(peer.parse().unwrap())), "{peer}");
        }
        assert!(!host_token_ok(Some(&hash), &token, None), "unknown peer");
        assert!(host_token_ok(Some(&hash), &token, Some("[::1]:5000".parse().unwrap())));
        assert!(host_token_ok(Some(&hash), &token, Some("[::ffff:127.0.0.1]:5000".parse().unwrap())));
        assert!(!host_token_ok(None, &token, Some("127.0.0.1:5000".parse().unwrap())));
    }

    #[tokio::test]
    async fn host_token_never_reaches_trading_routes() {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let mut st = state(pool);
        let token = with_host_token(&mut st);
        assert_eq!(send(&st, "POST", "/orders/submit", Cred::Bearer(&token), Some(serde_json::json!({}))).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&st, "GET", "/portfolios", Cred::Bearer(&token), None).await.0, StatusCode::UNAUTHORIZED);
        assert!(host_context().principal_id.is_nil());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn host_token_accepted_from_loopback() {
        let pool = migrated_db().await;
        let mut st = state(pool.clone());
        let token = with_host_token(&mut st);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer(&token), None).await.0, StatusCode::OK);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer("not-the-token"), None).await.0, StatusCode::UNAUTHORIZED);
    }

    /// What `oms setup sync-broker` does: an admin mutation from the host,
    /// alongside a named admin. Audited as `host-cli`.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn host_token_admin_post_works_with_named_admins() {
        let pool = migrated_db().await;
        let mut st = state(pool.clone());
        let token = with_host_token(&mut st);
        mfa_admin(&st, &email()).await;
        let body = serde_json::json!({ "name": "Bo", "email": email(), "password": "a long enough passphrase", "role": "admin" });
        let (status, text) = send(&st, "POST", "/admin/users", Cred::Bearer(&token), Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        let id: Uuid = serde_json::from_str::<serde_json::Value>(&text).unwrap()["user_id"].as_str().unwrap().parse().unwrap();
        let (status, text) =
            send(&st, "PUT", &format!("/admin/users/{id}/admin"), Cred::Bearer(&token), Some(serde_json::json!({ "admin": false }))).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
        assert!(audit_rows(&pool, "user.added").await.contains(&(HOST_CLI_ACTOR.to_string(), Some(id))));
        assert_eq!(audit_rows(&pool, "admin.revoked").await, vec![(HOST_CLI_ACTOR.to_string(), Some(id))]);
    }
}
