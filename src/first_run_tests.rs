//! Sign-in and accounts, the Open WebUI way: the first signup becomes the
//! admin, self-signup with pending approval, SSO signup / merge-by-email,
//! password change, optional MFA (`[auth.local].require_mfa`), voluntary
//! TOTP and its removal, and the end of the shared admin password and invites.

use axum::body::to_bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::{Extension, Json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::accounts::{resolve_sso, SignupPolicy, SsoOutcome, SsoPolicy};
use crate::app_state::AppState;
use crate::authz_schema_tests::migrated_db;
use crate::local_auth_api::{self as la_api, CodeBody, LocalAuth, LoginBody, SignupBody};

const ORIGIN: &str = "http://localhost:3001";
const PASSWORD: &str = "correct horse battery staple";
const NEW_PASSWORD: &str = "a much longer brand new passphrase";

fn state(pool: PgPool, require_mfa: bool) -> AppState {
    let mut st = crate::handlers::tests::test_app_state(pool);
    st.session_config.public_base_url = Some(ORIGIN.to_string());
    st.require_mfa = require_mfa;
    st.signup = SignupPolicy { enabled: true, pending_by_default: false };
    st
}

fn la() -> LocalAuth {
    let key = crate::secrets::parse_master_key("base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
    LocalAuth::new(Some(key), true)
}

fn hdrs(cookie: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("origin", ORIGIN.parse().unwrap());
    if let Some(c) = cookie {
        h.insert("cookie", format!("oms_session={c}").parse().unwrap());
    }
    h
}

fn peer() -> Option<ConnectInfo<std::net::SocketAddr>> {
    Some(ConnectInfo("10.0.0.9:5000".parse().unwrap()))
}

fn cookie_of(resp: &axum::response::Response) -> String {
    let raw = resp.headers().get(axum::http::header::SET_COOKIE).unwrap().to_str().unwrap();
    raw.split(';').next().unwrap().split_once('=').unwrap().1.to_string()
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

/// One request through the full router, as a browser (Origin set) would send it.
/// Returns (status, body, the session cookie it set if any).
async fn send(
    st: &AppState,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, String, Option<String>) {
    let app = crate::build_app(st.clone(), None, true, false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{path}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut req = client.request(method.parse().unwrap(), url).header("origin", ORIGIN);
    if let Some(c) = cookie {
        req = req.header("cookie", format!("oms_session={c}"));
    }
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
    let set = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.split(';').next()?.split_once('=').map(|(_, v)| v.to_string()))
        .filter(|v| !v.is_empty());
    let text = resp.text().await.unwrap();
    server.abort();
    (status, text, set)
}

async fn login(st: &AppState, email: &str, password: &str) -> axum::response::Response {
    la_api::login(
        State(st.clone()),
        Extension(la()),
        peer(),
        hdrs(None),
        Json(LoginBody { email: email.into(), password: password.into() }),
    )
    .await
}

async fn session_mfa(pool: &PgPool, cookie: &str) -> Option<bool> {
    crate::sessions::lookup_session(pool, &crate::sessions::hash_session_token(cookie))
        .await
        .unwrap()
        .map(|s| s.mfa_verified_at.is_some())
}

async fn signup(st: &AppState, email: &str, password: &str) -> axum::response::Response {
    la_api::signup(
        State(st.clone()),
        Extension(la()),
        peer(),
        hdrs(None),
        Json(SignupBody { name: "Test".into(), email: email.into(), password: password.into() }),
    )
    .await
}

/// Sign `email` up and give it the trader portfolio role. Returns the
/// principal, the session cookie and the `next` step the server named.
async fn enrolled_trader(st: &AppState, email: &str) -> (Uuid, String, String) {
    let resp = signup(st, email, PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookie_of(&resp);
    let next = json(resp).await["next"].as_str().unwrap().to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM principal WHERE email = $1").bind(email).fetch_one(st.pool()).await.unwrap();
    sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, 'trader')").bind(id).execute(st.pool()).await.unwrap();
    (id, cookie, next)
}

async fn role_of(pool: &PgPool, email: &str) -> (String, bool) {
    sqlx::query_as(
        "SELECT p.status, EXISTS (SELECT 1 FROM principal_admin a WHERE a.principal_id = p.id) FROM principal p WHERE p.email = $1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn sso(subject: &str, email: &str, verified: bool) -> crate::oidc::VerifiedIdentity {
    crate::oidc::VerifiedIdentity {
        issuer: "https://idp.test".into(),
        subject: subject.into(),
        display_name: Some("Sam".into()),
        email: Some(email.into()),
        email_verified: verified,
        claims: serde_json::json!({}),
    }
}

/// Voluntarily enrol TOTP on `cookie`. Returns the otpauth URI and the rotated cookie.
async fn enrol_totp(st: &AppState, cookie: &str) -> (String, String) {
    let resp = la_api::totp_enroll(State(st.clone()), Extension(la()), hdrs(Some(cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let uri = json(resp).await["otpauth_uri"].as_str().unwrap().to_string();
    let resp =
        la_api::totp_confirm(State(st.clone()), Extension(la()), peer(), hdrs(Some(cookie)), Json(CodeBody { code: code(&uri, 0) })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    (uri, cookie_of(&resp))
}

fn code(uri: &str, offset: i64) -> String {
    let t = totp_rs::Totp::from_url(uri).unwrap();
    t.generate((chrono::Utc::now().timestamp() + offset) as u64).to_string()
}

fn email() -> String {
    format!("fr-{}@fund.test", Uuid::new_v4().simple())
}

/// The status an order submit by the session behind `cookie` gets, after
/// authorization: 503 means authorized (only routing fails in tests).
async fn submit_status(st: &AppState, cookie: &str, principal: Uuid) -> StatusCode {
    let pool = st.pool();
    let (portfolio, instrument) = crate::authz_route_tests::tradeable_portfolio(pool).await;
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade, can_view) \
         VALUES ($1, $2, $3, true, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal)
    .bind(portfolio)
    .execute(pool)
    .await
    .unwrap();
    let (ctx, _) = crate::auth::authenticate(pool, &hdrs(Some(cookie)), &st.session_config)
        .await
        .ok()
        .flatten()
        .expect("the session authenticates");
    crate::authz_route_tests::submit(pool, &ctx, portfolio, instrument).await.1
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn first_signup_becomes_admin() {
    let pool = migrated_db().await;
    let mut st = state(pool.clone(), false);
    st.signup = SignupPolicy { enabled: false, pending_by_default: true };
    let (status, text, _) = send(&st, "GET", "/auth/methods", None, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let m: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!((m["first_run"].as_bool(), m["signup"].as_bool()), (Some(true), Some(false)));

    // Even with sign-up off, the first account is allowed, and it is the admin.
    let first = email();
    let resp = signup(&st, &first, PASSWORD).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookie_of(&resp);
    assert_eq!(json(resp).await, serde_json::json!({ "role": "admin", "next": "done" }));
    assert_eq!(role_of(&pool, &first).await, ("ACTIVE".to_string(), true));
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_portfolio_grant g JOIN principal p ON p.id = g.principal_id WHERE p.email = $1")
        .bind(&first)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
    assert_eq!(send(&st, "GET", "/admin/users", Some(&cookie), None, None).await.0, StatusCode::OK);
    let (_, text, _) = send(&st, "GET", "/auth/me", Some(&cookie), None, None).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["role"], "admin");
    let (_, text, _) = send(&st, "GET", "/auth/methods", None, None, None).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["first_run"], false);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn concurrent_first_signups_create_exactly_one_admin() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let handles: Vec<_> = (0..8)
        .map(|_| email())
        .map(|e| {
            let st = st.clone();
            tokio::spawn(async move { signup(&st, &e, PASSWORD).await.status() })
        })
        .collect();
    for h in handles {
        assert_eq!(h.await.unwrap(), StatusCode::OK);
    }
    let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_admin").fetch_one(&pool).await.unwrap();
    assert_eq!(admins, 1, "exactly one first admin; the rest are ordinary sign-ups");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn signup_disabled_refused() {
    let pool = migrated_db().await;
    let mut st = state(pool.clone(), false);
    assert_eq!(signup(&st, &email(), PASSWORD).await.status(), StatusCode::OK, "the first admin");
    st.signup.enabled = false;
    let e = email();
    let (status, text, _) = send(&st, "POST", "/auth/local/signup", None, None,
        Some(serde_json::json!({ "name": "X", "email": e, "password": PASSWORD }))).await;
    assert_eq!((status, text.as_str()), (StatusCode::FORBIDDEN, r#"{"error":"signup_disabled"}"#));
    let (_, text, _) = send(&st, "GET", "/auth/methods", None, None, None).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["signup"], false);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM principal WHERE email = $1").bind(&e).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
    st.signup.enabled = true;
    assert_eq!(signup(&st, &e, "short").await.status(), StatusCode::BAD_REQUEST, "the password policy applies");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn pending_user_sees_nothing_until_approved() {
    let pool = migrated_db().await;
    let mut st = state(pool.clone(), false);
    let admin = cookie_of(&signup(&st, &email(), PASSWORD).await);
    st.signup = SignupPolicy { enabled: true, pending_by_default: true };
    let e = email();
    let resp = signup(&st, &e, PASSWORD).await;
    let cookie = cookie_of(&resp);
    assert_eq!(json(resp).await, serde_json::json!({ "role": "pending", "next": "done" }));
    assert_eq!(role_of(&pool, &e).await, ("PENDING".to_string(), false));

    let blocked = r#"{"error":"approval_pending"}"#;
    for (method, path, body) in [
        ("GET", "/portfolios", None),
        ("POST", "/orders/submit", Some(serde_json::json!({}))),
        ("GET", "/admin/users", None),
        ("POST", "/auth/local/totp/enroll", None),
        ("POST", "/auth/local/password", Some(serde_json::json!({ "current_password": PASSWORD, "new_password": NEW_PASSWORD }))),
    ] {
        let (status, text, _) = send(&st, method, path, Some(&cookie), None, body).await;
        assert_eq!((status, text.as_str()), (StatusCode::FORBIDDEN, blocked), "{method} {path}");
    }
    let (status, text, _) = send(&st, "GET", "/auth/me", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["role"], "pending");
    assert_eq!(send(&st, "GET", "/auth/methods", Some(&cookie), None, None).await.0, StatusCode::OK);

    // The admin sees and approves it; the same session then gets in.
    let (_, text, _) = send(&st, "GET", "/admin/users", Some(&admin), None, None).await;
    let users: serde_json::Value = serde_json::from_str(&text).unwrap();
    let row = users.as_array().unwrap().iter().find(|u| u["email"] == e.as_str()).unwrap().clone();
    assert_eq!(row["account_role"], "pending");
    let id = row["principal_id"].as_str().unwrap();
    let (status, text, _) = send(&st, "POST", &format!("/admin/users/{id}/approve"), Some(&admin), None, Some(serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert_eq!(send(&st, "GET", "/portfolios", Some(&cookie), None, None).await.0, StatusCode::OK);
    let (status, _, _) = send(&st, "POST", "/auth/logout", Some(&cookie), None, None).await;
    assert!(status.is_success());
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn oidc_signup_creates_pending_when_enabled() {
    let pool = migrated_db().await;
    let policy = SsoPolicy { enable_signup: true, merge_accounts_by_email: false, trust_email_verified: true, pending_by_default: true };
    let e = email();
    let out = resolve_sso(&pool, &sso("s-1", &e, true), policy).await.unwrap();
    let SsoOutcome::Created(id) = out else { panic!("{out:?}") };
    assert_eq!(role_of(&pool, &e).await, ("PENDING".to_string(), false));
    assert_eq!(resolve_sso(&pool, &sso("s-1", &e, true), policy).await.unwrap(), SsoOutcome::Known(id), "same identity next time");
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'signup.sso' AND target_principal_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audited, 1);
    // An unverified email is not recorded, and never matched.
    let out = resolve_sso(&pool, &sso("s-2", &email(), false), policy).await.unwrap();
    let SsoOutcome::Created(id2) = out else { panic!("{out:?}") };
    let stored: Option<String> = sqlx::query_scalar("SELECT email FROM principal WHERE id = $1").bind(id2).fetch_one(&pool).await.unwrap();
    assert_eq!(stored, None);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn oidc_unknown_refused_when_signup_disabled() {
    let pool = migrated_db().await;
    let policy = SsoPolicy { enable_signup: false, merge_accounts_by_email: false, trust_email_verified: true, pending_by_default: true };
    assert_eq!(resolve_sso(&pool, &sso("s-3", &email(), true), policy).await.unwrap(), SsoOutcome::NoAccount);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_identity WHERE subject = 's-3'").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn merge_by_email_links_only_verified_email() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    signup(&st, &e, PASSWORD).await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM principal WHERE email = $1").bind(&e).fetch_one(&pool).await.unwrap();
    let policy = SsoPolicy { enable_signup: false, merge_accounts_by_email: true, trust_email_verified: true, pending_by_default: true };
    assert_eq!(resolve_sso(&pool, &sso("m-1", &e.to_uppercase(), false), policy).await.unwrap(), SsoOutcome::NoAccount, "unverified");
    let untrusted = SsoPolicy { trust_email_verified: false, ..policy };
    assert_eq!(resolve_sso(&pool, &sso("m-1", &e, true), untrusted).await.unwrap(), SsoOutcome::NoAccount, "provider not trusted");
    assert_eq!(resolve_sso(&pool, &sso("m-1", &e.to_uppercase(), true), policy).await.unwrap(), SsoOutcome::Merged(id));
    assert_eq!(resolve_sso(&pool, &sso("m-1", &e, true), policy).await.unwrap(), SsoOutcome::Known(id));
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'sso.merged_by_email' AND target_principal_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn merge_disabled_never_links() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    signup(&st, &e, PASSWORD).await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM principal WHERE email = $1").bind(&e).fetch_one(&pool).await.unwrap();
    let off = SsoPolicy { enable_signup: false, merge_accounts_by_email: false, trust_email_verified: true, pending_by_default: true };
    assert_eq!(resolve_sso(&pool, &sso("n-1", &e, true), off).await.unwrap(), SsoOutcome::NoAccount);
    // With sign-up on, a matching email gets a separate account, never the existing one.
    let signup_on = SsoPolicy { enable_signup: true, ..off };
    let out = resolve_sso(&pool, &sso("n-2", &e, true), signup_on).await.unwrap();
    assert!(matches!(out, SsoOutcome::Created(other) if other != id), "{out:?}");
    let linked: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_identity WHERE principal_id = $1 AND issuer = 'https://idp.test'")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(linked, 0);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn password_change_applies_policy_and_rotates_session() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let old = cookie_of(&signup(&st, &e, PASSWORD).await);

    let wrong = serde_json::json!({ "current_password": "nope", "new_password": NEW_PASSWORD });
    let (status, text, _) = send(&st, "POST", "/auth/local/password", Some(&old), None, Some(wrong)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    for weak in ["short", "films+pic+galeries"] {
        let body = serde_json::json!({ "current_password": PASSWORD, "new_password": weak });
        let (status, text, _) = send(&st, "POST", "/auth/local/password", Some(&old), None, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{weak}: {text}");
        assert!(text.contains("weak_password"), "{text}");
    }
    let ok = serde_json::json!({ "current_password": PASSWORD, "new_password": NEW_PASSWORD });
    let (status, text, new) = send(&st, "POST", "/auth/local/password", Some(&old), None, Some(ok)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), serde_json::json!({ "next": "done" }));
    let new = new.expect("a rotated cookie");
    assert_ne!(new, old);
    assert_eq!(session_mfa(&pool, &old).await, None, "the old session is revoked");
    assert_eq!(send(&st, "GET", "/admin/users", Some(&new), None, None).await.0, StatusCode::OK);
    assert_eq!(login(&st, &e, PASSWORD).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(login(&st, &e, NEW_PASSWORD).await.status(), StatusCode::OK);
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'local.password_changed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audited, 1);
}

#[tokio::test]
#[ignore = "requires a live Postgres (seeded); run with --ignored"]
async fn mfa_optional_by_default_allows_order_without_totp() {
    let st = state(crate::handlers::tests::test_pool().await, false);
    let (id, cookie, next) = enrolled_trader(&st, &email()).await;
    assert_eq!(next, "done");
    assert_eq!(submit_status(&st, &cookie, id).await, StatusCode::SERVICE_UNAVAILABLE, "authorized; only routing fails");
}

/// Whether the session behind `cookie` passes the MFA gate on order and
/// admin mutations. TOTP tests run on their own database: a TOTP secret
/// sealed with the test key must never land in the shared one.
async fn passes_mfa_gate(st: &AppState, cookie: &str) -> bool {
    let (ctx, _) = crate::auth::authenticate(st.pool(), &hdrs(Some(cookie)), &st.session_config)
        .await
        .ok()
        .flatten()
        .expect("the session authenticates");
    crate::authz::require_mfa(&ctx).is_ok()
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn mfa_required_config_restores_strict_mode() {
    let st = state(migrated_db().await, true);
    let (_, cookie, next) = enrolled_trader(&st, &email()).await;
    assert_eq!(next, "totp_enroll");
    assert_eq!(session_mfa(st.pool(), &cookie).await, Some(false));
    assert!(!passes_mfa_gate(&st, &cookie).await);
    let (_, cookie) = enrol_totp(&st, &cookie).await;
    assert_eq!(session_mfa(st.pool(), &cookie).await, Some(true));
    assert!(passes_mfa_gate(&st, &cookie).await);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn enrolled_totp_still_required_at_login_when_optional() {
    let st = state(migrated_db().await, false);
    let e = email();
    let (_, cookie, _) = enrolled_trader(&st, &e).await;
    let (uri, _) = enrol_totp(&st, &cookie).await;

    let resp = login(&st, &e, PASSWORD).await;
    let cookie = cookie_of(&resp);
    assert_eq!(json(resp).await, serde_json::json!({ "next": "totp" }));
    assert_eq!(session_mfa(st.pool(), &cookie).await, Some(false));
    assert!(!passes_mfa_gate(&st, &cookie).await);

    let resp = la_api::totp(State(st.clone()), Extension(la()), peer(), hdrs(Some(&cookie)), Json(CodeBody { code: code(&uri, 30) })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(passes_mfa_gate(&st, &cookie_of(&resp)).await);
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn totp_disable_needs_valid_code() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let (id, cookie, _) = enrolled_trader(&st, &e).await;
    let (uri, cookie) = enrol_totp(&st, &cookie).await;
    let disable = |st: &AppState, cookie: &str, code: String| {
        la_api::totp_disable(State(st.clone()), Extension(la()), peer(), hdrs(Some(cookie)), Json(CodeBody { code }))
    };

    let resp = disable(&st, &cookie, "000000".into()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let enrolled: bool = sqlx::query_scalar("SELECT totp_enrolled_at IS NOT NULL FROM local_credential WHERE principal_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(enrolled, "a wrong code changes nothing");

    // Strict mode refuses to drop the second factor at all.
    let strict = state(pool.clone(), true);
    assert_eq!(disable(&strict, &cookie, code(&uri, 30)).await.status(), StatusCode::CONFLICT);

    let resp = disable(&st, &cookie, code(&uri, 30)).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (secret, enrolled, codes): (bool, bool, i64) = sqlx::query_as(
        "SELECT totp_secret_enc IS NOT NULL, totp_enrolled_at IS NOT NULL, \
                (SELECT count(*) FROM recovery_code r WHERE r.principal_id = $1) \
           FROM local_credential WHERE principal_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((secret, enrolled, codes), (false, false, 0));
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'local.totp_disabled' AND target_principal_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audited, 1);
    assert_eq!(json(login(&st, &e, PASSWORD).await).await, serde_json::json!({ "next": "done" }));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn signup_skips_totp_when_optional() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let (_, cookie, next) = enrolled_trader(&st, &email()).await;
    assert_eq!(next, "done");
    assert_eq!(session_mfa(&pool, &cookie).await, Some(true), "the session satisfies the (optional) MFA policy");
    let (_, cookie, next) = enrolled_trader(&state(pool.clone(), true), &email()).await;
    assert_eq!(next, "totp_enroll");
    assert_eq!(session_mfa(&pool, &cookie).await, Some(false));
}

/// The old shared password (`[server].admin_password`, `OMS_ADMIN_PASSWORD`)
/// opens nothing, whatever the old `legacy_admin_enabled` switch says.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_bearer_no_longer_accepted() {
    let pool = migrated_db().await;
    sqlx::query("UPDATE system_setting SET value = 'true'::jsonb WHERE key = 'legacy_admin_enabled'").execute(&pool).await.unwrap();
    std::env::set_var("OMS_ADMIN_PASSWORD", "legacy-shared-pw");
    let st = state(pool, false);
    for pw in ["legacy-shared-pw", "openoms-dev", "test-admin-token"] {
        let (status, _, _) = send(&st, "GET", "/admin/users", None, Some(pw), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{pw}");
        let body = serde_json::json!({ "name": "X", "email": email(), "password": PASSWORD, "role": "admin" });
        let (status, _, _) = send(&st, "POST", "/admin/users", None, Some(pw), Some(body)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{pw}");
    }
    std::env::remove_var("OMS_ADMIN_PASSWORD");
}

/// Invites are gone end to end.
#[tokio::test]
async fn removed_invite_routes_404() {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://u:p@127.0.0.1:1/none").unwrap();
    let st = state(pool, false);
    for (method, path) in [
        ("GET", "/auth/invite/abc/preview"),
        ("POST", "/auth/local/enroll"),
        ("POST", "/admin/users/invite"),
        ("POST", "/admin/users/invites/00000000-0000-0000-0000-000000000000/reissue"),
        ("GET", "/admin/users/bindings"),
        ("POST", "/admin/users/bootstrap-admin"),
    ] {
        let (status, _, _) = send(&st, method, path, None, None, Some(serde_json::json!({}))).await;
        assert!(status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::UNAUTHORIZED,
            "{method} {path}: {status}");
    }
}

/// A host-CLI password (bootstrap or recover) is someone else's choice: the
/// session it opens can only change it.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn cli_password_must_be_changed_before_anything_else() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let (_, password) = crate::admin_bootstrap::bootstrap(&pool, &e, None, crate::admin_bootstrap::HOST_CLI_ACTOR, true).await.unwrap();
    let resp = login(&st, &e, &password).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = cookie_of(&resp);
    assert_eq!(json(resp).await, serde_json::json!({ "next": "change_password" }));

    let blocked = r#"{"error":"password_change_required"}"#;
    for (method, path, body) in [
        ("GET", "/admin/users", None),
        ("GET", "/portfolios", None),
        ("POST", "/orders/submit", Some(serde_json::json!({}))),
        ("POST", "/auth/local/totp/enroll", None),
        ("POST", "/auth/logout-all", None),
    ] {
        let (status, text, _) = send(&st, method, path, Some(&cookie), None, body).await;
        assert_eq!((status, text.as_str()), (StatusCode::FORBIDDEN, blocked), "{method} {path}");
    }
    let (status, text, _) = send(&st, "GET", "/auth/me", Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["must_change_password"], true);
    assert_eq!(send(&st, "GET", "/auth/methods", Some(&cookie), None, None).await.0, StatusCode::OK);

    let change = serde_json::json!({ "current_password": password, "new_password": NEW_PASSWORD });
    let (status, text, new) = send(&st, "POST", "/auth/local/password", Some(&cookie), None, Some(change)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let new = new.unwrap();
    assert_eq!(send(&st, "GET", "/admin/users", Some(&new), None, None).await.0, StatusCode::OK);
    let (_, text, _) = send(&st, "GET", "/auth/me", Some(&new), None, None).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["must_change_password"], false);

    // Recovery hands out another such password.
    let (_, again) = crate::admin_bootstrap::recover(&pool, &e, crate::admin_bootstrap::HOST_CLI_ACTOR).await.unwrap();
    assert_eq!(json(login(&st, &e, &again).await).await, serde_json::json!({ "next": "change_password" }));
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn cli_password_expires_after_24h() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let (id, password) = crate::admin_bootstrap::bootstrap(&pool, &e, None, crate::admin_bootstrap::HOST_CLI_ACTOR, true).await.unwrap();
    let expires: bool = sqlx::query_scalar(
        "SELECT password_expires_at BETWEEN now() + interval '23 hours' AND now() + interval '25 hours' FROM local_credential WHERE principal_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(expires, "valid for 24 h");
    sqlx::query("UPDATE local_credential SET password_expires_at = now() - interval '1 minute' WHERE principal_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let resp = login(&st, &e, &password).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json(resp).await, serde_json::json!({ "error": "invalid_credentials" }));

    // Used in time, it no longer expires (the change is still required).
    let (_, password) = crate::admin_bootstrap::recover(&pool, &e, crate::admin_bootstrap::HOST_CLI_ACTOR).await.unwrap();
    assert_eq!(login(&st, &e, &password).await.status(), StatusCode::OK);
    let cleared: bool = sqlx::query_scalar("SELECT password_expires_at IS NULL FROM local_credential WHERE principal_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(cleared);
}

/// A password change ends every other session of the user.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn password_change_revokes_other_sessions() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let first = cookie_of(&signup(&st, &e, PASSWORD).await);
    let other = cookie_of(&login(&st, &e, PASSWORD).await);
    let mut auth = st.event_bus().subscribe_auth();
    let change = serde_json::json!({ "current_password": PASSWORD, "new_password": NEW_PASSWORD });
    assert_eq!(send(&st, "POST", "/auth/local/password", Some(&other), None, Some(change)).await.0, StatusCode::OK);
    assert_eq!(session_mfa(&pool, &first).await, None, "the other session is revoked");
    assert!(auth.try_recv().is_ok(), "AuthChanged published");
}

#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn totp_disable_consumes_the_code_and_announces_it() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    let (_, cookie, _) = enrolled_trader(&st, &e).await;
    let (uri, cookie) = enrol_totp(&st, &cookie).await;
    let used = code(&uri, 0);
    // The confirm consumed this step already: replaying it must fail.
    let resp = la_api::totp_disable(State(st.clone()), Extension(la()), peer(), hdrs(Some(&cookie)), Json(CodeBody { code: used })).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "a consumed code cannot disable TOTP");
    let mut auth = st.event_bus().subscribe_auth();
    let resp = la_api::totp_disable(State(st.clone()), Extension(la()), peer(), hdrs(Some(&cookie)), Json(CodeBody { code: code(&uri, 30) })).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(auth.try_recv().is_ok(), "AuthChanged published");
}

/// Sessions minted under the removed shared password (`auth_method =
/// 'legacy_admin'`) resolve to no session: 401, never a 500.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn legacy_admin_sessions_are_no_session() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let e = email();
    signup(&st, &e, PASSWORD).await;
    let token = "legacy-session-token-0123456789";
    sqlx::query(
        "INSERT INTO user_session (principal_id, token_hash, absolute_expires_at, auth_method) \
         SELECT id, $2, now() + interval '1 day', 'legacy_admin' FROM principal WHERE email = $1",
    )
    .bind(&e)
    .bind(crate::sessions::hash_session_token(token))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(send(&st, "GET", "/auth/me", Some(token), None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&st, "GET", "/admin/users", Some(token), None, None).await.0, StatusCode::UNAUTHORIZED);
}

/// Open WebUI's per-user keys: a signed-in user lists, creates and revokes
/// their own keys, which act with that user's access; nobody else's.
#[tokio::test]
#[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
async fn user_can_manage_own_api_keys() {
    let pool = migrated_db().await;
    let st = state(pool.clone(), false);
    let me = email();
    signup(&st, &email(), PASSWORD).await; // the admin
    let cookie = cookie_of(&signup(&st, &me, PASSWORD).await);
    let my_id: Uuid = sqlx::query_scalar("SELECT id FROM principal WHERE email = $1").bind(&me).fetch_one(&pool).await.unwrap();

    let (status, text, _) = send(&st, "POST", "/auth/keys", Some(&cookie), None, Some(serde_json::json!({ "name": "laptop" }))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let key: serde_json::Value = serde_json::from_str(&text).unwrap();
    let (key_id, secret) = (key["key_id"].as_str().unwrap().to_string(), key["secret"].as_str().unwrap().to_string());
    let (who, _) = crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().expect("the key works");
    assert_eq!(who, my_id, "it acts as me");

    let (_, text, _) = send(&st, "GET", "/auth/keys", Some(&cookie), None, None).await;
    let keys: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(keys.as_array().unwrap().len(), 1);
    assert!(keys[0].get("secret").is_none_or(|s| s.is_null()), "the secret is shown once");

    // A key cannot mint keys.
    let bearer = format!("{key_id}.{secret}");
    let (status, _, _) = send(&st, "POST", "/auth/keys", None, Some(&bearer), Some(serde_json::json!({ "name": "x" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Someone else's key is not mine to revoke.
    let other = email();
    let other_cookie = cookie_of(&signup(&st, &other, PASSWORD).await);
    let (status, _, _) = send(&st, "DELETE", &format!("/auth/keys/{key_id}"), Some(&other_cookie), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, _) = send(&st, "DELETE", &format!("/auth/keys/{key_id}"), Some(&cookie), None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(crate::auth::verify_key(&pool, &key_id, &secret).await.unwrap().is_none());
}
