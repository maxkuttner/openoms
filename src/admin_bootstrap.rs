//! Named admins: the legacy shared-password transition, first-admin bootstrap
//! and break-glass recovery.
//!
//! Admin is a global capability (a `principal_admin` row), separate from roles
//! and portfolio grants: a bootstrapped admin can run the console and cannot
//! trade until someone gives them a role and grants.
//!
//! The deprecated shared password keeps working only while
//! `system_setting.legacy_admin_enabled` is true, and only as the synthetic
//! `legacy-admin` (reads, plus creating a named admin). The first time a named
//! admin completes a local sign-in with TOTP the flag flips to false, once.

use axum::http::Method;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::invites::{self, NewInvite};

/// Audit actor of the legacy shared password.
pub const LEGACY_ACTOR: &str = "legacy-admin";
/// Audit actor of the host CLI (`oms auth …`, `oms init`).
pub const HOST_CLI_ACTOR: &str = "host-cli";
/// The only mutation the legacy password may make.
pub const BOOTSTRAP_PATH: &str = "/admin/users/bootstrap-admin";

/// The synthetic context of the legacy password. The nil UUID never exists in
/// `principal`, so no authz or trading check can ever pass for it.
pub fn legacy_context() -> AuthContext {
    AuthContext { principal_id: Uuid::nil(), principal_code: LEGACY_ACTOR.to_string() }
}

/// What `legacy-admin` may do: read anything under `/admin`, and create a
/// named admin.
pub fn legacy_may(method: &Method, path: &str) -> bool {
    crate::sessions::is_read_method(method) || (method == Method::POST && path == BOOTSTRAP_PATH)
}

pub async fn legacy_admin_enabled(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM system_setting WHERE key = 'legacy_admin_enabled'")
            .fetch_optional(pool)
            .await?;
    Ok(value == Some(serde_json::Value::Bool(true)))
}

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

/// The cutover. Flips `legacy_admin_enabled` to false when `principal_id` is
/// an ACTIVE admin holding a live local session that passed TOTP; audits
/// `admin.legacy_disabled` exactly once (the conditional UPDATE is the
/// guard, so two concurrent sign-ins cannot both audit). Returns whether this
/// call did the flip.
pub async fn disable_legacy_after_mfa(pool: &PgPool, principal_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let flipped: Option<String> = sqlx::query_scalar(
        "UPDATE system_setting SET value = 'false'::jsonb, updated_at = now() \
          WHERE key = 'legacy_admin_enabled' AND value = 'true'::jsonb \
            AND EXISTS (SELECT 1 FROM principal_admin a JOIN principal p ON p.id = a.principal_id \
                         WHERE a.principal_id = $1 AND p.status = 'ACTIVE') \
            AND EXISTS (SELECT 1 FROM user_session s \
                         WHERE s.principal_id = $1 AND s.auth_method = 'local' \
                           AND s.mfa_verified_at IS NOT NULL AND s.revoked_at IS NULL) \
         RETURNING (SELECT code FROM principal WHERE id = $1)",
    )
    .bind(principal_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(code) = flipped else {
        return Ok(false);
    };
    invites::audit(&mut tx, &code, "admin.legacy_disabled", principal_id, serde_json::json!({})).await?;
    tx.commit().await?;
    Ok(true)
}

#[derive(Debug)]
pub enum BootstrapError {
    /// An ACTIVE admin exists and recovery was not forced.
    AdminExists,
    /// A user (not disabled) already has this email.
    EmailTaken,
    /// No admin (not disabled) has this email.
    NotAnAdmin,
    BadEmail,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for BootstrapError {
    fn from(err: sqlx::Error) -> Self {
        BootstrapError::Db(err)
    }
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::AdminExists => f.write_str(
                "an active admin already exists; sign in as them, or use \
                 `oms auth recover-admin --email …` (or --force-recovery) if nobody can",
            ),
            BootstrapError::EmailTaken => f.write_str("a user with this email already exists"),
            BootstrapError::NotAnAdmin => f.write_str("no admin with this email"),
            BootstrapError::BadEmail => f.write_str("a valid email is required"),
            BootstrapError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

/// Create a named admin: an INVITED HUMAN with the admin capability, no role
/// and no portfolio grants, plus a local-only invite whose link enrols a
/// password and TOTP. With `only_if_no_admin`, refused while any ACTIVE admin
/// exists (the host CLI's rule). Audited `admin.bootstrap` by `actor`.
pub async fn bootstrap(
    pool: &PgPool,
    email: &str,
    display_name: Option<&str>,
    actor: &str,
    only_if_no_admin: bool,
) -> Result<NewInvite, BootstrapError> {
    let email = normalise_email(email)?;
    let display_name = display_name.map(str::trim).filter(|n| !n.is_empty());
    let mut tx = pool.begin().await?;
    lock_admins(&mut tx).await?;
    if only_if_no_admin && other_active_admins(&mut tx, Uuid::nil()).await? > 0 {
        return Err(BootstrapError::AdminExists);
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal WHERE lower(email) = $1 AND status <> 'DISABLED') \
             OR EXISTS (SELECT 1 FROM invite WHERE lower(email) = $1 AND accepted_at IS NULL \
                         AND revoked_at IS NULL AND expires_at > now())",
    )
    .bind(&email)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(BootstrapError::EmailTaken);
    }
    let code = invites::generate_unique_code(&mut tx, email.split('@').next().unwrap_or_default()).await?;
    let principal_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO principal (id, code, principal_type, status, email, display_name) \
         VALUES ($1, $2, 'HUMAN', 'INVITED', $3, $4)",
    )
    .bind(principal_id)
    .bind(&code)
    .bind(&email)
    .bind(display_name)
    .execute(&mut *tx)
    .await;
    if let Err(err) = inserted {
        // A concurrent invite for the same address won the unique index.
        return Err(match &err {
            sqlx::Error::Database(d) if d.is_unique_violation() => BootstrapError::EmailTaken,
            _ => err.into(),
        });
    }
    sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, $2)")
        .bind(principal_id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    let invite = local_invite(&mut tx, principal_id, &email, actor).await?;
    invites::audit(
        &mut tx,
        actor,
        "admin.bootstrap",
        principal_id,
        serde_json::json!({ "invite_id": invite.invite_id, "email": email, "forced": !only_if_no_admin }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite)
}

fn normalise_email(email: &str) -> Result<String, BootstrapError> {
    let email = email.trim().to_lowercase();
    match email.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() && !email.contains(char::is_whitespace) => Ok(email),
        _ => Err(BootstrapError::BadEmail),
    }
}

/// An invite that only a local password + TOTP enrolment can claim.
async fn local_invite(
    tx: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    email: &str,
    actor: &str,
) -> Result<NewInvite, sqlx::Error> {
    let invite = invites::insert_invite(tx, principal_id, email, actor).await?;
    sqlx::query("UPDATE invite SET method = 'local' WHERE id = $1").bind(invite.invite_id).execute(&mut **tx).await?;
    Ok(invite)
}

/// Break-glass: reset the admin's local password and TOTP, revoke their
/// sessions and pending invites, and issue a new local-only enrolment invite.
/// The admin is INVITED until they enrol again. Audited `admin.recovered`.
///
/// Recovering the sole admin leaves zero ACTIVE admins until they re-enrol,
/// and the legacy shared password stays disabled if the cutover already
/// happened: the new link (or the host CLI) is the only way back in.
pub async fn recover(pool: &PgPool, email: &str, actor: &str) -> Result<NewInvite, BootstrapError> {
    let email = normalise_email(email)?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT p.id, p.email FROM principal p JOIN principal_admin a ON a.principal_id = p.id \
          WHERE lower(p.email) = $1 AND p.principal_type = 'HUMAN' AND p.status <> 'DISABLED' \
          FOR UPDATE OF p",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BootstrapError::NotAnAdmin)?;
    let principal_id: Uuid = row.get("id");
    let stored_email: String = row.get("email");

    for sql in [
        "DELETE FROM recovery_code WHERE principal_id = $1",
        "DELETE FROM local_credential WHERE principal_id = $1",
        "UPDATE invite SET revoked_at = now() WHERE principal_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
        "UPDATE principal SET status = 'INVITED', updated_at = now() WHERE id = $1",
    ] {
        sqlx::query(sql).bind(principal_id).execute(&mut *tx).await?;
    }
    // The local identity goes with the credential; enrolling links it afresh.
    sqlx::query("DELETE FROM principal_identity WHERE principal_id = $1 AND issuer = $2")
        .bind(principal_id)
        .bind(crate::local_auth_api::LOCAL_ISSUER)
        .execute(&mut *tx)
        .await?;
    let sessions = sqlx::query("UPDATE user_session SET revoked_at = now() WHERE principal_id = $1 AND revoked_at IS NULL")
        .bind(principal_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let invite = local_invite(&mut tx, principal_id, &stored_email, actor).await?;
    invites::audit(
        &mut tx,
        actor,
        "admin.recovered",
        principal_id,
        serde_json::json!({ "invite_id": invite.invite_id, "sessions_revoked": sessions }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite)
}

// ── host token ─────────────────────────────────────────────────────────────

/// File name of the per-boot host token, next to the config file in effect.
pub const HOST_TOKEN_FILE: &str = "host-token";

/// Where the host token lives: beside the `oms.toml` in effect, else beside
/// the one `oms init` would create. Server and CLI resolve it identically.
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
            "cannot read the host token at {} ({e}): run this on the OMS host as the OMS user, \
             with the server running",
            path.display()
        )
    })
}

/// The synthetic context of the host token: actor `host-cli`, the nil UUID
/// (never a `principal`), so it can never pass a trading or authz check.
pub fn host_context() -> AuthContext {
    AuthContext { principal_id: Uuid::nil(), principal_code: HOST_CLI_ACTOR.to_string() }
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

/// The enrolment link for `invite`, the same shape the invite API returns.
pub fn enrolment_link(base: &str, invite: &NewInvite) -> String {
    format!("{}/auth/invite/{}", base.trim_end_matches('/'), invite.token)
}

// ── HTTP ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BootstrapRequest {
    pub email: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BootstrapResponse {
    pub user_id: Uuid,
    pub link: String,
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

/// Where enrolment links point: `[auth].public_base_url`, else the loopback
/// bind address the server would derive it from.
pub fn link_base(configured: Option<String>, bind: &str) -> Result<String, String> {
    match configured.filter(|u| !u.trim().is_empty()) {
        Some(url) => Ok(url),
        None => crate::sessions::derive_public_base_url(bind).map_err(|_| {
            format!(
                "OMS_BIND_ADDR ({bind}) is reachable beyond this machine and no public URL is \
                 configured, so there is no address to put in the link. Set [auth].public_base_url \
                 in oms.toml to the URL browsers use to reach this server."
            )
        }),
    }
}

fn cli_link_base() -> Result<String, String> {
    let file = crate::config::load();
    let bind = crate::resolve_bind_addr(std::env::var("OMS_BIND_ADDR").ok(), file);
    link_base(file.and_then(|f| f.auth_settings().public_base_url), &bind)
}

/// `oms auth bootstrap-admin`. Returns the enrolment link.
pub async fn cli_bootstrap(
    o: crate::setup::database::config::PostgresOverrides,
    email: &str,
    name: Option<&str>,
    force_recovery: bool,
) -> Result<String, String> {
    let base = cli_link_base()?;
    let pool = cli_pool(o).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    let invite = bootstrap(&pool, email, name, HOST_CLI_ACTOR, !force_recovery).await.map_err(|e| e.to_string())?;
    Ok(enrolment_link(&base, &invite))
}

/// `oms auth recover-admin`. Returns the new enrolment link.
pub async fn cli_recover(o: crate::setup::database::config::PostgresOverrides, email: &str) -> Result<String, String> {
    let base = cli_link_base()?;
    let pool = cli_pool(o).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    let invite = recover(&pool, email, HOST_CLI_ACTOR).await.map_err(|e| e.to_string())?;
    Ok(enrolment_link(&base, &invite))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::authz_schema_tests::migrated_db;
    use crate::local_auth_api::{self as la_api, CodeBody, EnrollBody, LocalAuth};
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::{Extension, Json};

    const ORIGIN: &str = "http://localhost:3001";
    const LEGACY_PW: &str = "legacy-shared-pw";
    const PASSWORD: &str = "correct horse battery staple";

    fn state(pool: PgPool, legacy: Option<&str>) -> AppState {
        let mut st = crate::handlers::tests::test_app_state(pool);
        st.session_config.public_base_url = Some(ORIGIN.to_string());
        st.legacy_admin_password = legacy.map(str::to_string);
        st
    }

    fn la() -> LocalAuth {
        let key = crate::secrets::parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        LocalAuth::new(Some(key), true, false)
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

    /// Enrol a bootstrapped admin with a password only: a local session with
    /// MFA still pending. Returns (principal_id, cookie).
    async fn enrolled_admin(st: &AppState, la: &LocalAuth, email: &str) -> (Uuid, String) {
        let invite = bootstrap(st.pool(), email, Some("Ada"), HOST_CLI_ACTOR, false).await.unwrap();
        let resp = la_api::enroll(
            State(st.clone()),
            Extension(la.clone()),
            hdrs(None),
            Json(EnrollBody { token: invite.token, password: PASSWORD.into() }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        (invite.principal_id, cookie_of(&resp))
    }

    /// Finish TOTP enrolment on `cookie`; returns the MFA-verified cookie.
    async fn verify_totp(st: &AppState, la: &LocalAuth, cookie: &str) -> String {
        let resp = la_api::totp_enroll(State(st.clone()), Extension(la.clone()), hdrs(Some(cookie))).await;
        let uri = json(resp).await["otpauth_uri"].as_str().unwrap().to_string();
        let code = totp_rs::Totp::from_url(&uri).unwrap().generate(chrono::Utc::now().timestamp() as u64).to_string();
        let resp = la_api::totp_confirm(State(st.clone()), Extension(la.clone()), None, hdrs(Some(cookie)), Json(CodeBody { code })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        cookie_of(&resp)
    }

    async fn mfa_admin(st: &AppState, email: &str) -> (Uuid, String) {
        let la = la();
        let (id, cookie) = enrolled_admin(st, &la, email).await;
        (id, verify_totp(st, &la, &cookie).await)
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
    async fn legacy_admin_can_bootstrap_named_admin() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), Some(LEGACY_PW));
        let e = email();

        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer(LEGACY_PW), None).await.0, StatusCode::OK);
        let (status, body) = send(
            &st,
            "POST",
            BOOTSTRAP_PATH,
            Cred::Bearer(LEGACY_PW),
            Some(serde_json::json!({ "email": e, "display_name": "Ada" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(body["link"].as_str().unwrap().starts_with(&format!("{ORIGIN}/auth/invite/")), "{body}");
        let id: Uuid = body["user_id"].as_str().unwrap().parse().unwrap();

        let (status, kind, name): (String, String, Option<String>) =
            sqlx::query_as("SELECT status, principal_type, display_name FROM principal WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((status.as_str(), kind.as_str(), name.as_deref()), ("INVITED", "HUMAN", Some("Ada")));
        let granted_by: String = sqlx::query_scalar("SELECT granted_by FROM principal_admin WHERE principal_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(granted_by, LEGACY_ACTOR);
        let (roles, grants): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM principal_role WHERE principal_id = $1), \
                    (SELECT count(*) FROM principal_portfolio_grant WHERE principal_id = $1)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((roles, grants), (0, 0), "admin is not a trading role");
        let method: String = sqlx::query_scalar("SELECT method FROM invite WHERE principal_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(method, "local");
        assert_eq!(audit_rows(&pool, "admin.bootstrap").await, vec![(LEGACY_ACTOR.to_string(), Some(id))]);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn legacy_admin_cannot_mutate_otherwise() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), Some(LEGACY_PW));
        let target = bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, false).await.unwrap().principal_id;

        let (status, body) = send(
            &st,
            "POST",
            "/admin/portfolios",
            Cred::Bearer(LEGACY_PW),
            Some(serde_json::json!({ "code": "p", "name": "p" })),
        )
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "create a named admin first"));
        let (status, body) = send(
            &st,
            "PUT",
            &format!("/admin/users/{target}/admin"),
            Cred::Bearer(LEGACY_PW),
            Some(serde_json::json!({ "admin": false })),
        )
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "create a named admin first"));
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer("wrong"), None).await.0, StatusCode::UNAUTHORIZED);

        // The synthetic actor never reaches trading or authz routes.
        let submit = serde_json::json!({});
        assert_eq!(send(&st, "POST", "/orders/submit", Cred::Bearer(LEGACY_PW), Some(submit)).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&st, "GET", "/portfolios", Cred::Bearer(LEGACY_PW), None).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn legacy_admin_refused_after_named_admin_mfa_login() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), Some(LEGACY_PW));
        let la = la();
        let (id, cookie) = enrolled_admin(&st, &la, &email()).await;
        assert!(legacy_admin_enabled(&pool).await.unwrap(), "a password alone does not cut over");

        let mfa_cookie = verify_totp(&st, &la, &cookie).await;
        assert!(!legacy_admin_enabled(&pool).await.unwrap());
        let code: String = sqlx::query_scalar("SELECT code FROM principal WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(audit_rows(&pool, "admin.legacy_disabled").await, vec![(code, Some(id))]);

        let (status, _) = send(&st, "GET", "/admin/users", Cred::Bearer(LEGACY_PW), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(&st, "POST", BOOTSTRAP_PATH, Cred::Bearer(LEGACY_PW), Some(serde_json::json!({ "email": email() }))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&mfa_cookie), None).await.0, StatusCode::OK);

        // A second admin's MFA sign-in neither re-flips nor re-audits.
        mfa_admin(&st, &email()).await;
        assert_eq!(audit_rows(&pool, "admin.legacy_disabled").await.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_routes_need_admin_capability() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), None);
        let pm = pm_session(&pool).await;
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&pm), None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::None, None).await.0, StatusCode::UNAUTHORIZED);

        let (_, admin) = mfa_admin(&st, &email()).await;
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&admin), None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn local_admin_needs_mfa_for_mutations() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), None);
        let la = la();
        let (_, cookie) = enrolled_admin(&st, &la, &email()).await;
        let new_admin = serde_json::json!({ "email": email() });

        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&cookie), None).await.0, StatusCode::OK, "reads need no MFA");
        let (status, body) = send(&st, "POST", BOOTSTRAP_PATH, Cred::Cookie(&cookie), Some(new_admin.clone())).await;
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "mfa_required"));
        let cookie = verify_totp(&st, &la, &cookie).await;
        assert_eq!(send(&st, "POST", BOOTSTRAP_PATH, Cred::Cookie(&cookie), Some(new_admin)).await.0, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn admin_does_not_imply_trading() {
        // The seeded database: placing an order needs its reference data.
        let pool = crate::handlers::tests::test_pool().await;
        let st = state(pool.clone(), None);
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
        let st = state(pool.clone(), None);
        // An INVITED admin does not count: they may never enrol.
        bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, true).await.unwrap();
        bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, true).await.unwrap();
        mfa_admin(&st, &email()).await;

        assert!(matches!(bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, true).await, Err(BootstrapError::AdminExists)));
        let forced = bootstrap(&pool, &email(), None, HOST_CLI_ACTOR, false).await.unwrap();
        assert!(audit_rows(&pool, "admin.bootstrap").await.contains(&(HOST_CLI_ACTOR.to_string(), Some(forced.principal_id))));

        let taken = email();
        bootstrap(&pool, &taken, None, HOST_CLI_ACTOR, false).await.unwrap();
        assert!(matches!(bootstrap(&pool, &taken, None, HOST_CLI_ACTOR, false).await, Err(BootstrapError::EmailTaken)));
        assert!(matches!(bootstrap(&pool, "nope", None, HOST_CLI_ACTOR, false).await, Err(BootstrapError::BadEmail)));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn cli_recover_revokes_sessions() {
        let pool = migrated_db().await;
        let st = state(pool.clone(), None);
        let e = email();
        let (id, cookie) = mfa_admin(&st, &e).await;

        let invite = recover(&pool, &e.to_uppercase(), HOST_CLI_ACTOR).await.unwrap();
        assert_eq!(invite.principal_id, id);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&cookie), None).await.0, StatusCode::UNAUTHORIZED);
        let (live, creds, codes): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM user_session WHERE principal_id = $1 AND revoked_at IS NULL), \
                    (SELECT count(*) FROM local_credential WHERE principal_id = $1), \
                    (SELECT count(*) FROM recovery_code WHERE principal_id = $1)",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((live, creds, codes), (0, 0, 0));
        assert_eq!(audit_rows(&pool, "admin.recovered").await, vec![(HOST_CLI_ACTOR.to_string(), Some(id))]);

        // The new link enrols the same admin again, password and TOTP afresh.
        let la = la();
        let resp = la_api::enroll(
            State(st.clone()),
            Extension(la.clone()),
            hdrs(None),
            Json(EnrollBody { token: invite.token, password: "another long passphrase".into() }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = verify_totp(&st, &la, &cookie_of(&resp)).await;
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Cookie(&cookie), None).await.0, StatusCode::OK);

        assert!(matches!(recover(&pool, &email(), HOST_CLI_ACTOR).await, Err(BootstrapError::NotAnAdmin)));
    }

    /// No shared password configured means none is accepted — in particular
    /// not the old built-in `openoms-dev`, on a loopback bind or anywhere.
    #[tokio::test]
    async fn no_default_admin_password_on_loopback() {
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
        let st = state(pool, None);
        assert!(crate::bind_is_loopback("localhost:3001"));
        assert_eq!(crate::resolve_admin_password(None, None), None);
        for pw in ["openoms-dev", ""] {
            assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer(pw), None).await.0, StatusCode::UNAUTHORIZED, "{pw:?}");
        }
    }

    #[test]
    fn legacy_may_only_read_and_bootstrap() {
        assert!(legacy_may(&Method::GET, "/admin/users"));
        assert!(legacy_may(&Method::POST, BOOTSTRAP_PATH));
        assert!(!legacy_may(&Method::POST, "/admin/users/invite"));
        assert!(!legacy_may(&Method::PUT, BOOTSTRAP_PATH));
        assert!(!legacy_may(&Method::DELETE, "/admin/users/x/keys/y"));
        assert!(legacy_context().principal_id.is_nil());
    }

    #[test]
    fn link_base_needs_a_url_off_loopback() {
        assert_eq!(link_base(None, "localhost:3001").unwrap(), "http://localhost:3001");
        assert_eq!(link_base(Some("https://oms.example.com".into()), "0.0.0.0:3001").unwrap(), "https://oms.example.com");
        assert!(link_base(None, "0.0.0.0:3001").is_err());
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
        let mut st = state(pool, None);
        let token = with_host_token(&mut st);
        assert_eq!(send(&st, "POST", "/orders/submit", Cred::Bearer(&token), Some(serde_json::json!({}))).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&st, "GET", "/portfolios", Cred::Bearer(&token), None).await.0, StatusCode::UNAUTHORIZED);
        assert!(host_context().principal_id.is_nil());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn host_token_accepted_from_loopback() {
        let pool = migrated_db().await;
        let mut st = state(pool.clone(), Some(LEGACY_PW));
        let token = with_host_token(&mut st);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer(&token), None).await.0, StatusCode::OK);
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer("not-the-token"), None).await.0, StatusCode::UNAUTHORIZED);
    }

    /// What `oms setup sync-broker` does: an admin mutation from the host,
    /// after the legacy password has been cut over. Audited as `host-cli`.
    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn host_token_admin_post_works_after_cutover() {
        let pool = migrated_db().await;
        let mut st = state(pool.clone(), Some(LEGACY_PW));
        let token = with_host_token(&mut st);
        mfa_admin(&st, &email()).await;
        assert!(!legacy_admin_enabled(&pool).await.unwrap());
        assert_eq!(send(&st, "GET", "/admin/users", Cred::Bearer(LEGACY_PW), None).await.0, StatusCode::FORBIDDEN);

        let (status, body) =
            send(&st, "POST", BOOTSTRAP_PATH, Cred::Bearer(&token), Some(serde_json::json!({ "email": email() }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id: Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["user_id"].as_str().unwrap().parse().unwrap();
        let (status, body) = send(
            &st,
            "PUT",
            &format!("/admin/users/{id}/admin"),
            Cred::Bearer(&token),
            Some(serde_json::json!({ "admin": false })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        assert!(audit_rows(&pool, "admin.bootstrap").await.contains(&(HOST_CLI_ACTOR.to_string(), Some(id))));
        assert_eq!(audit_rows(&pool, "admin.revoked").await, vec![(HOST_CLI_ACTOR.to_string(), Some(id))]);
    }
}
