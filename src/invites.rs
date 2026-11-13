//! Invite-only sign-in.
//!
//! An admin invites an email address with a role and portfolio grants. That
//! creates the user right away (status `INVITED`) plus a one-time link token,
//! of which only the SHA-256 is stored. The first OIDC login that arrives with
//! the token and a **verified** matching email claims it: the login's
//! `(issuer, subject)` is linked to the user and the user becomes `ACTIVE`.
//! Every later login resolves by `(issuer, subject)` alone; identities are
//! never matched or merged on email, and a login with no valid invite creates
//! nothing.

use chrono::{DateTime, Utc};
use rand::RngCore;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::oidc::VerifiedIdentity;
use crate::sessions::hash_session_token;

/// How long an invite link stays claimable.
const INVITE_TTL_DAYS: i64 = 7;

/// Roles a person can be invited with. `strategy` belongs to algorithms only.
const INVITABLE_ROLES: [&str; 3] = ["pm", "trader", "viewer"];

/// Whether `role` is one a person (invited or key-only) can hold.
pub fn is_assignable_role(role: &str) -> bool {
    INVITABLE_ROLES.contains(&role)
}

/// A freshly minted invite. `token` is the plaintext link token: it is
/// returned exactly once and never stored.
#[derive(Debug, Clone)]
pub struct NewInvite {
    pub invite_id: Uuid,
    pub principal_id: Uuid,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum InviteError {
    /// The role is not one a person can be invited with.
    BadRole,
    /// A pending invite (or an invited user) already exists for this email.
    AlreadyInvited,
    /// An active user already has this email.
    AlreadyUser,
    /// No such invite, or it can no longer be revoked / re-issued.
    NotFound,
    /// A link invite needs an active person with an email on record.
    NotLinkable,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for InviteError {
    fn from(err: sqlx::Error) -> Self {
        InviteError::Db(err)
    }
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InviteError::BadRole => write!(f, "role must be pm, trader or viewer"),
            InviteError::AlreadyInvited => write!(f, "this email already has a pending invite"),
            InviteError::AlreadyUser => write!(f, "a user with this email already exists"),
            InviteError::NotFound => write!(f, "invite not found or no longer pending"),
            InviteError::NotLinkable => write!(f, "user is not active or has no email"),
            InviteError::Db(err) => write!(f, "database error: {err}"),
        }
    }
}

impl std::error::Error for InviteError {}

/// What a login resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The login claimed an invite: its identity is now linked.
    Linked { principal_id: Uuid, code: String },
    /// The identity was already linked to an active user.
    Known { principal_id: Uuid, code: String },
    NoInvite,
    Expired,
    Revoked,
    /// The verified email is not the invited one; `invited_hint` is the
    /// invited address, masked (`j…@fundco.com`).
    EmailMismatch { invited_hint: String },
    EmailUnverified,
}

impl ClaimOutcome {
    /// The `reason` shown on the access-refused page, or `None` for a login
    /// that succeeded.
    pub fn refusal_reason(&self) -> Option<&'static str> {
        match self {
            ClaimOutcome::Linked { .. } | ClaimOutcome::Known { .. } => None,
            ClaimOutcome::NoInvite => Some("no_invite"),
            ClaimOutcome::Expired => Some("expired"),
            ClaimOutcome::Revoked => Some("revoked"),
            ClaimOutcome::EmailMismatch { .. } => Some("email_mismatch"),
            ClaimOutcome::EmailUnverified => Some("email_unverified"),
        }
    }
}

/// Invite `email` as a new user with `role` and portfolio `grants`
/// (`(portfolio_id, can_trade)`; every grant can view).
pub async fn create(
    pool: &PgPool,
    email: &str,
    role: &str,
    grants: &[(Uuid, bool)],
    created_by: &str,
) -> Result<NewInvite, InviteError> {
    if !INVITABLE_ROLES.contains(&role) {
        return Err(InviteError::BadRole);
    }
    let email = email.trim().to_lowercase();

    let mut tx = pool.begin().await?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT status FROM principal WHERE email = $1 AND status <> 'DISABLED'",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    match existing.as_deref() {
        Some("ACTIVE") => return Err(InviteError::AlreadyUser),
        Some(_) => return Err(InviteError::AlreadyInvited),
        None => {}
    }
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM invite WHERE email = $1 AND accepted_at IS NULL \
           AND revoked_at IS NULL AND expires_at > now())",
    )
    .bind(&email)
    .fetch_one(&mut *tx)
    .await?;
    if pending {
        return Err(InviteError::AlreadyInvited);
    }

    let local = email.split('@').next().unwrap_or_default();
    let code = generate_unique_code(&mut tx, local).await?;
    let principal_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO principal (id, code, principal_type, status, email) \
         VALUES ($1, $2, 'HUMAN', 'INVITED', $3)",
    )
    .bind(principal_id)
    .bind(&code)
    .bind(&email)
    .execute(&mut *tx)
    .await;
    if let Err(err) = inserted {
        // A concurrent invite for the same address won the unique index.
        return Err(if is_unique_violation(&err) { InviteError::AlreadyInvited } else { err.into() });
    }
    sqlx::query("INSERT INTO principal_role (principal_id, role_code) VALUES ($1, $2)")
        .bind(principal_id)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    for (portfolio_id, can_trade) in grants {
        sqlx::query(
            // can_allocate follows can_trade: one toggle for the admin.
            "INSERT INTO principal_portfolio_grant \
                (id, principal_id, portfolio_id, can_trade, can_view, can_allocate) \
             VALUES ($1, $2, $3, $4, true, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(principal_id)
        .bind(portfolio_id)
        .bind(can_trade)
        .execute(&mut *tx)
        .await?;
    }

    let invite = insert_invite(&mut tx, principal_id, &email, created_by).await?;
    let grant_detail: Vec<serde_json::Value> = grants
        .iter()
        .map(|(p, t)| serde_json::json!({ "portfolio_id": p, "can_trade": t }))
        .collect();
    audit(
        &mut tx,
        created_by,
        "invite.created",
        principal_id,
        serde_json::json!({ "invite_id": invite.invite_id, "email": email, "role": role, "grants": grant_detail }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite)
}

/// Revoke a pending invite. An invited user who never signed in becomes
/// `DISABLED`; an active user's link invite just stops working.
pub async fn revoke(pool: &PgPool, invite_id: Uuid, actor: &str) -> Result<(), InviteError> {
    let mut tx = pool.begin().await?;
    let principal_id: Uuid = sqlx::query_scalar(
        "UPDATE invite SET revoked_at = now() \
          WHERE id = $1 AND accepted_at IS NULL AND revoked_at IS NULL \
         RETURNING principal_id",
    )
    .bind(invite_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(InviteError::NotFound)?;
    sqlx::query(
        "UPDATE principal SET status = 'DISABLED', updated_at = now() \
          WHERE id = $1 AND status = 'INVITED'",
    )
    .bind(principal_id)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "invite.revoked", principal_id, serde_json::json!({ "invite_id": invite_id })).await?;
    tx.commit().await?;
    Ok(())
}

/// A new token and expiry for the same user. Every older pending invite stops
/// working, and only the user's newest unclaimed invite can be reissued;
/// a user disabled by revoking an unclaimed invite is invited again.
pub async fn reissue(pool: &PgPool, invite_id: Uuid, actor: &str) -> Result<NewInvite, InviteError> {
    let mut tx = pool.begin().await?;
    // Only an unclaimed invite, and only the user's newest one: an invite that
    // a later reissue already replaced must not mint a rival to the live one.
    let row = sqlx::query(
        "SELECT principal_id, email FROM invite i \
          WHERE id = $1 AND accepted_at IS NULL \
            AND created_at = (SELECT max(created_at) FROM invite WHERE principal_id = i.principal_id) \
            FOR UPDATE",
    )
    .bind(invite_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(InviteError::NotFound)?;
    let principal_id: Uuid = row.get("principal_id");
    let email: String = row.get("email");

    // The user never has two live invites: retire whatever is still pending.
    sqlx::query(
        "UPDATE invite SET revoked_at = now() \
          WHERE principal_id = $1 AND accepted_at IS NULL AND revoked_at IS NULL",
    )
    .bind(principal_id)
    .execute(&mut *tx)
    .await?;
    let reinvited = sqlx::query(
        "UPDATE principal SET status = 'INVITED', updated_at = now() \
          WHERE id = $1 AND status = 'DISABLED' \
            AND NOT EXISTS (SELECT 1 FROM principal_identity WHERE principal_id = $1)",
    )
    .bind(principal_id)
    .execute(&mut *tx)
    .await;
    if let Err(err) = reinvited {
        // Someone else now holds this address.
        return Err(if is_unique_violation(&err) { InviteError::AlreadyInvited } else { err.into() });
    }

    let invite = insert_invite(&mut tx, principal_id, &email, actor).await?;
    audit(
        &mut tx,
        actor,
        "invite.reissued",
        principal_id,
        serde_json::json!({ "invite_id": invite.invite_id, "replaces": invite_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite)
}

/// An invite for an active user to link a second sign-in provider. The email
/// is the one on record; claiming it adds an identity and changes nothing else.
pub async fn create_link(pool: &PgPool, principal_id: Uuid, actor: &str) -> Result<NewInvite, InviteError> {
    let mut tx = pool.begin().await?;
    let email: String = sqlx::query_scalar(
        "SELECT email FROM principal \
          WHERE id = $1 AND principal_type = 'HUMAN' AND status = 'ACTIVE' AND email IS NOT NULL",
    )
    .bind(principal_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(InviteError::NotLinkable)?;
    let invite = insert_invite(&mut tx, principal_id, &email, actor).await?;
    audit(
        &mut tx,
        actor,
        "invite.created",
        principal_id,
        serde_json::json!({ "invite_id": invite.invite_id, "email": email, "link": true }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite)
}

/// Resolve a verified login. A linked identity resolves by `(issuer, subject)`
/// only; an unlinked one can only get in by claiming the invite whose token
/// came with the login flow. Every refusal leaves the database unchanged.
pub async fn resolve_login(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    invite_token: Option<&str>,
) -> Result<ClaimOutcome, sqlx::Error> {
    let known = sqlx::query(
        "SELECT p.id, p.code, p.principal_type, p.status \
           FROM principal_identity i JOIN principal p ON p.id = i.principal_id \
          WHERE i.issuer = $1 AND i.subject = $2",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = known {
        let kind: String = row.get("principal_type");
        let status: String = row.get("status");
        // A linked identity never claims anything: a strategy, invited or
        // disabled user behind it is simply refused.
        if kind != "HUMAN" || status != "ACTIVE" {
            return Ok(ClaimOutcome::NoInvite);
        }
        sqlx::query("UPDATE principal_identity SET last_login_at = now() WHERE issuer = $1 AND subject = $2")
            .bind(&identity.issuer)
            .bind(&identity.subject)
            .execute(pool)
            .await?;
        return Ok(ClaimOutcome::Known { principal_id: row.get("id"), code: row.get("code") });
    }

    let Some(token) = invite_token else {
        return Ok(ClaimOutcome::NoInvite);
    };

    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query(
        "SELECT i.id, i.email, i.principal_id, i.expires_at, i.accepted_at, i.revoked_at, \
                p.code, p.status \
           FROM invite i JOIN principal p ON p.id = i.principal_id \
          WHERE i.token_hash = $1 \
          FOR UPDATE OF i",
    )
    .bind(hash_session_token(token))
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(ClaimOutcome::NoInvite);
    };
    let invite_id: Uuid = row.get("id");
    let invited_email: String = row.get("email");
    let principal_id: Uuid = row.get("principal_id");
    let code: String = row.get("code");
    let status: String = row.get("status");
    let expires_at: DateTime<Utc> = row.get("expires_at");
    let accepted_at: Option<DateTime<Utc>> = row.get("accepted_at");
    let revoked_at: Option<DateTime<Utc>> = row.get("revoked_at");

    if accepted_at.is_some() {
        return Ok(ClaimOutcome::NoInvite);
    }
    if revoked_at.is_some() || status == "DISABLED" {
        return Ok(ClaimOutcome::Revoked);
    }
    if expires_at <= Utc::now() {
        return Ok(ClaimOutcome::Expired);
    }
    let verified_email = identity.email.as_deref().filter(|_| identity.email_verified);
    let Some(verified_email) = verified_email else {
        return Ok(ClaimOutcome::EmailUnverified);
    };
    if !verified_email.trim().eq_ignore_ascii_case(&invited_email) {
        return Ok(ClaimOutcome::EmailMismatch { invited_hint: mask_email(&invited_email) });
    }

    sqlx::query(
        "INSERT INTO principal_identity (issuer, subject, principal_id, last_login_at) \
         VALUES ($1, $2, $3, now())",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .bind(principal_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE principal SET status = 'ACTIVE', \
                display_name = COALESCE(display_name, $2), updated_at = now() \
          WHERE id = $1",
    )
    .bind(principal_id)
    .bind(&identity.display_name)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE invite SET accepted_at = now() WHERE id = $1")
        .bind(invite_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &code,
        "invite.accepted",
        principal_id,
        serde_json::json!({ "invite_id": invite_id, "issuer": identity.issuer, "subject": identity.subject }),
    )
    .await?;
    tx.commit().await?;
    Ok(ClaimOutcome::Linked { principal_id, code })
}

/// `jane@fundco.com` → `j…@fundco.com`.
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}…@{domain}")
        }
        None => "…".to_string(),
    }
}

pub(crate) async fn insert_invite(
    tx: &mut Transaction<'_, Postgres>,
    principal_id: Uuid,
    email: &str,
    created_by: &str,
) -> Result<NewInvite, sqlx::Error> {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let invite_id = Uuid::new_v4();
    let expires_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO invite (id, token_hash, email, principal_id, created_by, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(days => $6)) RETURNING expires_at",
    )
    .bind(invite_id)
    .bind(hash_session_token(&token))
    .bind(email)
    .bind(principal_id)
    .bind(created_by)
    .bind(INVITE_TTL_DAYS as i32)
    .fetch_one(&mut **tx)
    .await?;
    Ok(NewInvite { invite_id, principal_id, token, expires_at })
}

pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    action: &str,
    target: Uuid,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_event (actor, action, target_principal_id, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Slugifies `seed` and, if that code is taken, appends the lowest free
/// numeric suffix (`-2`, `-3`, ...), so admins see a readable name.
pub(crate) async fn generate_unique_code(
    tx: &mut Transaction<'_, Postgres>,
    seed: &str,
) -> Result<String, sqlx::Error> {
    let base = slugify(seed);
    let base = if base.is_empty() { "user".to_string() } else { base };
    let mut candidate = base.clone();
    let mut suffix = 1u32;
    loop {
        let taken: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principal WHERE code = $1)")
            .bind(&candidate)
            .fetch_one(&mut **tx)
            .await?;
        if !taken {
            return Ok(candidate);
        }
        suffix += 1;
        candidate = format!("{base}-{suffix}");
    }
}

/// Lowercase alphanumerics joined by single hyphens.
fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz_schema_tests::migrated_db;

    const ISSUER: &str = "https://idp.test";

    fn login(issuer: &str, subject: &str, email: &str, verified: bool) -> VerifiedIdentity {
        VerifiedIdentity {
            issuer: issuer.to_string(),
            subject: subject.to_string(),
            display_name: Some("Jane".to_string()),
            email: Some(email.to_string()),
            email_verified: verified,
            claims: serde_json::json!({}),
        }
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn status(pool: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM principal WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
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

    async fn invite_jane(pool: &PgPool) -> NewInvite {
        create(pool, "Jane@FundCo.com", "trader", &[], "admin").await.unwrap()
    }

    #[test]
    fn the_hint_masks_the_local_part() {
        assert_eq!(mask_email("jane@fundco.com"), "j…@fundco.com");
    }

    #[test]
    fn strategy_and_unknown_roles_cannot_be_invited() {
        assert!(!INVITABLE_ROLES.contains(&"strategy"));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn create_makes_an_invited_user_with_role_grants_and_audit() {
        let pool = migrated_db().await;
        let p = portfolio(&pool).await;
        let invite = create(&pool, "Jane.Doe@FundCo.com", "pm", &[(p, true)], "admin").await.unwrap();

        let row = sqlx::query("SELECT code, status, email, principal_type FROM principal WHERE id = $1")
            .bind(invite.principal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("code"), "jane-doe");
        assert_eq!(row.get::<String, _>("status"), "INVITED");
        assert_eq!(row.get::<String, _>("email"), "jane.doe@fundco.com");
        assert_eq!(row.get::<String, _>("principal_type"), "HUMAN");
        let role: String = sqlx::query_scalar("SELECT role_code FROM principal_role WHERE principal_id = $1")
            .bind(invite.principal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(role, "pm");
        let (can_view, can_trade): (bool, bool) = sqlx::query_as(
            "SELECT can_view, can_trade FROM principal_portfolio_grant WHERE principal_id = $1 AND portfolio_id = $2",
        )
        .bind(invite.principal_id)
        .bind(p)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(can_view && can_trade);
        let days = (invite.expires_at - Utc::now()).num_hours();
        assert!((7 * 24 - 1..=7 * 24).contains(&days), "expiry is 7 days, got {days}h");
        let actor: String = sqlx::query_scalar(
            "SELECT actor FROM audit_event WHERE action = 'invite.created' AND target_principal_id = $1",
        )
        .bind(invite.principal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(actor, "admin");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn bad_roles_are_refused() {
        let pool = migrated_db().await;
        for role in ["strategy", "admin"] {
            let err = create(&pool, "x@fundco.com", role, &[], "admin").await.unwrap_err();
            assert!(matches!(err, InviteError::BadRole), "{role}");
        }
        assert_eq!(count(&pool, "invite").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn duplicate_pending_invite_is_refused() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let err = create(&pool, "JANE@fundco.com", "viewer", &[], "admin").await.unwrap_err();
        assert!(matches!(err, InviteError::AlreadyInvited), "{err}");

        resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        let err = create(&pool, "jane@fundco.com", "viewer", &[], "admin").await.unwrap_err();
        assert!(matches!(err, InviteError::AlreadyUser), "{err}");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn uninvited_login_creates_no_rows() {
        let pool = migrated_db().await;
        let (principals, identities) = (count(&pool, "principal").await, count(&pool, "principal_identity").await);

        let outcome = resolve_login(&pool, &login(ISSUER, "stranger", "x@y.com", true), None).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::NoInvite);
        let outcome = resolve_login(&pool, &login(ISSUER, "stranger", "x@y.com", true), Some("forged"))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::NoInvite);

        assert_eq!(count(&pool, "principal").await, principals);
        assert_eq!(count(&pool, "principal_identity").await, identities);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invite_claim_links_identity_and_activates() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let identity = login(ISSUER, "s1", "jane@fundco.com", true);

        let outcome = resolve_login(&pool, &identity, Some(&invite.token)).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::Linked { principal_id: invite.principal_id, code: "jane".into() });
        assert_eq!(status(&pool, invite.principal_id).await, "ACTIVE");
        let accepted: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT accepted_at FROM invite WHERE id = $1")
            .bind(invite.invite_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(accepted.is_some());
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_event WHERE action = 'invite.accepted' AND target_principal_id = $1",
        )
        .bind(invite.principal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audited, 1);

        // Later logins resolve by (issuer, subject), without any token.
        let again = resolve_login(&pool, &identity, None).await.unwrap();
        assert_eq!(again, ClaimOutcome::Known { principal_id: invite.principal_id, code: "jane".into() });
        let last_login: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT last_login_at FROM principal_identity WHERE issuer = $1 AND subject = 's1'",
        )
        .bind(ISSUER)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(last_login.is_some());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn email_mismatch_is_refused_and_invite_stays_pending() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;

        let outcome = resolve_login(&pool, &login(ISSUER, "s1", "bob@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::EmailMismatch { invited_hint: "j…@fundco.com".into() });
        assert_eq!(status(&pool, invite.principal_id).await, "INVITED");
        assert_eq!(count(&pool, "principal_identity").await, 0);

        // Still claimable by the right address.
        let outcome = resolve_login(&pool, &login(ISSUER, "s2", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Linked { .. }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn email_compare_is_case_insensitive() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let outcome = resolve_login(
            &pool,
            &login(ISSUER, "s1", "JANE@fundCO.COM", true),
            Some(&invite.token),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Linked { .. }), "{outcome:?}");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn unverified_email_is_refused() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let outcome = resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", false), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::EmailUnverified);
        assert_eq!(status(&pool, invite.principal_id).await, "INVITED");
        assert_eq!(count(&pool, "principal_identity").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn expired_invite_is_refused() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        sqlx::query("UPDATE invite SET expires_at = now() - interval '1 minute' WHERE id = $1")
            .bind(invite.invite_id)
            .execute(&pool)
            .await
            .unwrap();
        let outcome = resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Expired);
        assert_eq!(count(&pool, "principal_identity").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn revoked_invite_is_refused() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        revoke(&pool, invite.invite_id, "admin").await.unwrap();
        assert_eq!(status(&pool, invite.principal_id).await, "DISABLED");

        let outcome = resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Revoked);
        assert_eq!(count(&pool, "principal_identity").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn reissue_replaces_the_token_and_reinvites() {
        let pool = migrated_db().await;
        let old = invite_jane(&pool).await;
        revoke(&pool, old.invite_id, "admin").await.unwrap();

        let new = reissue(&pool, old.invite_id, "admin").await.unwrap();
        assert_eq!(new.principal_id, old.principal_id);
        assert_ne!(new.token, old.token);
        assert_eq!(status(&pool, old.principal_id).await, "INVITED");

        let identity = login(ISSUER, "s1", "jane@fundco.com", true);
        assert_eq!(resolve_login(&pool, &identity, Some(&old.token)).await.unwrap(), ClaimOutcome::Revoked);
        assert!(matches!(
            resolve_login(&pool, &identity, Some(&new.token)).await.unwrap(),
            ClaimOutcome::Linked { .. }
        ));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn reissue_never_leaves_two_live_invites() {
        let pool = migrated_db().await;
        let first = invite_jane(&pool).await;
        let second = reissue(&pool, first.invite_id, "admin").await.unwrap();
        // The first invite was superseded; reissuing it again must not mint a rival.
        assert!(matches!(reissue(&pool, first.invite_id, "admin").await, Err(InviteError::NotFound)));
        let third = reissue(&pool, second.invite_id, "admin").await.unwrap();
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM invite WHERE principal_id = $1 \
               AND accepted_at IS NULL AND revoked_at IS NULL",
        )
        .bind(first.principal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(live, 1);

        // An accepted invite cannot be reissued.
        let identity = login(ISSUER, "s9", "jane@fundco.com", true);
        resolve_login(&pool, &identity, Some(&third.token)).await.unwrap();
        assert!(matches!(reissue(&pool, third.invite_id, "admin").await, Err(InviteError::NotFound)));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn second_provider_same_email_is_not_merged() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();

        let other = login("https://github.test", "gh-1", "jane@fundco.com", true);
        assert_eq!(resolve_login(&pool, &other, None).await.unwrap(), ClaimOutcome::NoInvite);
        assert_eq!(count(&pool, "principal_identity").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invite_token_is_single_use() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        let second = login("https://github.test", "gh-1", "jane@fundco.com", true);
        assert_eq!(resolve_login(&pool, &second, Some(&invite.token)).await.unwrap(), ClaimOutcome::NoInvite);
        assert_eq!(count(&pool, "principal_identity").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn token_stored_hashed_only() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let found: i64 = sqlx::query_scalar("SELECT count(*) FROM invite i WHERE i::text LIKE '%' || $1 || '%'")
            .bind(&invite.token)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(found, 0, "the plaintext token must not be stored");
        let hash: String = sqlx::query_scalar("SELECT token_hash FROM invite WHERE id = $1")
            .bind(invite.invite_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(hash, hash_session_token(&invite.token));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn link_invite_adds_second_identity() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        resolve_login(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();

        let link = create_link(&pool, invite.principal_id, "admin").await.unwrap();
        assert_eq!(link.principal_id, invite.principal_id);
        let github = login("https://github.test", "gh-1", "jane@fundco.com", true);
        let outcome = resolve_login(&pool, &github, Some(&link.token)).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::Linked { principal_id: invite.principal_id, code: "jane".into() });
        assert_eq!(status(&pool, invite.principal_id).await, "ACTIVE");
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM principal_identity WHERE principal_id = $1")
            .bind(invite.principal_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 2);
    }
}
