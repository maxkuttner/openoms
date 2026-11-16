//! Accounts: who may get one and how (the Open WebUI model).
//!
//! - **First signup becomes the admin.** While no admin exists at all, the
//!   sign-in page offers "Create the admin account"; that account gets the
//!   admin capability. Decided under the admin lock, so concurrent first
//!   signups produce exactly one admin (the others are ordinary signups).
//! - **Self-signup** (`[auth].enable_signup`, default on) creates an account
//!   with `[auth].default_user_role`: `pending` (default; signed in, sees
//!   nothing until an admin approves it) or `user`.
//! - **Admin "Add user"** creates a local account directly with a password the
//!   admin passes on out of band.
//! - **SSO** accounts come only from `[auth.oidc].enable_signup` (an unknown
//!   identity creates an account with the default role) or
//!   `merge_accounts_by_email` (a *verified* email matching an existing account
//!   links to it). Otherwise an unknown identity is refused.
//!
//! The account role is derived, not stored: `admin` = a `principal_admin` row,
//! `pending` = status `PENDING`, anything else active = `user`.

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::oidc::VerifiedIdentity;

/// The account-level role (admin / user / pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    Admin,
    User,
    Pending,
}

impl AccountRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountRole::Admin => "admin",
            AccountRole::User => "user",
            AccountRole::Pending => "pending",
        }
    }
}

#[derive(Debug)]
pub enum AccountError {
    /// A live account already has this email.
    EmailTaken,
    BadEmail,
    /// Self-signup is off and this is not the first account.
    SignupDisabled,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AccountError {
    fn from(err: sqlx::Error) -> Self {
        AccountError::Db(err)
    }
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountError::EmailTaken => f.write_str("an account with this email already exists"),
            AccountError::BadEmail => f.write_str("a valid email is required"),
            AccountError::SignupDisabled => f.write_str("sign-up is turned off"),
            AccountError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for AccountError {}

/// Trimmed, lowercased, and shaped like an address.
pub fn normalise_email(email: &str) -> Result<String, AccountError> {
    let email = email.trim().to_lowercase();
    match email.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() && !email.contains(char::is_whitespace) => Ok(email),
        _ => Err(AccountError::BadEmail),
    }
}

/// A new account. `password_hash` makes it a local account (with its local
/// identity); `None` is for an SSO account, whose identity the caller links.
pub struct NewAccount<'a> {
    pub name: Option<&'a str>,
    /// Already normalised (see `normalise_email`), or `None` (SSO without a verified email).
    pub email: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub role: AccountRole,
}

/// Insert the account inside `tx` and audit `action` by `actor`. Returns the
/// new principal's id and code.
pub async fn create_account(
    tx: &mut Transaction<'_, Postgres>,
    acct: &NewAccount<'_>,
    actor: &str,
    action: &str,
) -> Result<(Uuid, String), AccountError> {
    if let Some(email) = acct.email {
        let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principal WHERE lower(email) = $1 AND status <> 'DISABLED')")
            .bind(email)
            .fetch_one(&mut **tx)
            .await?;
        if taken {
            return Err(AccountError::EmailTaken);
        }
    }
    let seed = acct.email.and_then(|e| e.split('@').next()).or(acct.name).unwrap_or("user");
    let code = generate_unique_code(tx, seed).await?;
    let id = Uuid::new_v4();
    let status = if acct.role == AccountRole::Pending { "PENDING" } else { "ACTIVE" };
    let name = acct.name.map(str::trim).filter(|n| !n.is_empty());
    let inserted = sqlx::query(
        "INSERT INTO principal (id, code, principal_type, status, email, display_name) VALUES ($1, $2, 'HUMAN', $3, $4, $5)",
    )
    .bind(id)
    .bind(&code)
    .bind(status)
    .bind(acct.email)
    .bind(name)
    .execute(&mut **tx)
    .await;
    if let Err(err) = inserted {
        return Err(if is_unique_violation(&err) { AccountError::EmailTaken } else { err.into() });
    }
    if acct.role == AccountRole::Admin {
        sqlx::query("INSERT INTO principal_admin (principal_id, granted_by) VALUES ($1, $2)")
            .bind(id)
            .bind(actor)
            .execute(&mut **tx)
            .await?;
    }
    if let Some(hash) = acct.password_hash {
        sqlx::query("INSERT INTO local_credential (principal_id, password_hash) VALUES ($1, $2)")
            .bind(id)
            .bind(hash)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO principal_identity (issuer, subject, principal_id) VALUES ($1, $2, $3)")
            .bind(crate::local_auth_api::LOCAL_ISSUER)
            .bind(id.to_string())
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    audit(tx, actor, action, id, serde_json::json!({ "email": acct.email, "role": acct.role.as_str() })).await?;
    Ok((id, code))
}

/// What a self-signup produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signup {
    pub principal_id: Uuid,
    pub role: AccountRole,
}

/// The sign-up rules in force (`[auth]`).
#[derive(Debug, Clone, Copy)]
pub struct SignupPolicy {
    pub enabled: bool,
    /// `default_user_role = "pending"`.
    pub pending_by_default: bool,
}

impl SignupPolicy {
    fn default_role(self) -> AccountRole {
        if self.pending_by_default { AccountRole::Pending } else { AccountRole::User }
    }
}

impl Default for SignupPolicy {
    fn default() -> Self {
        SignupPolicy { enabled: true, pending_by_default: true }
    }
}

/// Is this a fresh install (no admin at all, not even a disabled one)?
pub async fn first_run(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM principal_admin)").fetch_one(pool).await
}

/// A local self-signup. The first account ever (no admin exists) becomes the
/// admin, whatever `policy` says; later ones need `policy.enabled` and get the
/// default role. Serialised on the admin lock, so of concurrent first signups
/// exactly one becomes the admin. Audited `signup.first_admin` / `signup.local`.
pub async fn signup(
    pool: &PgPool,
    name: &str,
    email: &str,
    password_hash: &str,
    policy: SignupPolicy,
) -> Result<Signup, AccountError> {
    let email = normalise_email(email)?;
    let mut tx = pool.begin().await?;
    crate::admin_bootstrap::lock_admins(&mut tx).await?;
    let first: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM principal_admin)").fetch_one(&mut *tx).await?;
    let role = if first {
        AccountRole::Admin
    } else if !policy.enabled {
        return Err(AccountError::SignupDisabled);
    } else {
        policy.default_role()
    };
    let action = if first { "signup.first_admin" } else { "signup.local" };
    let acct = NewAccount { name: Some(name), email: Some(&email), password_hash: Some(password_hash), role };
    let (principal_id, _) = create_account(&mut tx, &acct, &email, action).await?;
    tx.commit().await?;
    Ok(Signup { principal_id, role })
}

/// How SSO logins may get an account (`[auth.oidc]` + `[auth]`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SsoPolicy {
    pub enable_signup: bool,
    pub merge_accounts_by_email: bool,
    /// `[auth.oidc].trust_email_verified`: off, no email is ever trusted.
    pub trust_email_verified: bool,
    pub pending_by_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SsoOutcome {
    /// The identity was already linked to a live account.
    Known(Uuid),
    /// Linked to the existing account with the same verified email.
    Merged(Uuid),
    /// A new account was created for it.
    Created(Uuid),
    /// Unknown, and neither merge nor signup applies: no access.
    NoAccount,
}

impl SsoOutcome {
    pub fn principal_id(self) -> Option<Uuid> {
        match self {
            SsoOutcome::Known(id) | SsoOutcome::Merged(id) | SsoOutcome::Created(id) => Some(id),
            SsoOutcome::NoAccount => None,
        }
    }
}

/// Resolve an OIDC login to an account. A linked `(issuer, subject)` wins;
/// otherwise a verified email may merge (if allowed), else the identity may
/// sign up (if allowed), else nothing. Identities are keyed by
/// `(issuer, subject)` only; email is consulted solely for an opted-in merge.
pub async fn resolve_sso(pool: &PgPool, identity: &VerifiedIdentity, policy: SsoPolicy) -> Result<SsoOutcome, sqlx::Error> {
    let known = sqlx::query(
        "SELECT p.id, p.principal_type, p.status FROM principal_identity i JOIN principal p ON p.id = i.principal_id \
          WHERE i.issuer = $1 AND i.subject = $2",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = known {
        let kind: String = row.get("principal_type");
        let status: String = row.get("status");
        if kind != "HUMAN" || !(status == "ACTIVE" || status == "PENDING") {
            return Ok(SsoOutcome::NoAccount);
        }
        sqlx::query("UPDATE principal_identity SET last_login_at = now() WHERE issuer = $1 AND subject = $2")
            .bind(&identity.issuer)
            .bind(&identity.subject)
            .execute(pool)
            .await?;
        return Ok(SsoOutcome::Known(row.get("id")));
    }

    let verified_email = identity
        .email
        .as_deref()
        .filter(|_| identity.email_verified && policy.trust_email_verified)
        .and_then(|e| normalise_email(e).ok());

    if policy.merge_accounts_by_email {
        if let Some(email) = verified_email.as_deref() {
            let mut tx = pool.begin().await?;
            let target = sqlx::query(
                "SELECT id, code FROM principal WHERE lower(email) = $1 AND principal_type = 'HUMAN' \
                   AND status IN ('ACTIVE', 'PENDING') FOR UPDATE",
            )
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = target {
                let id: Uuid = row.get("id");
                let code: String = row.get("code");
                link_identity(&mut tx, identity, id).await?;
                audit(&mut tx, &code, "sso.merged_by_email", id, serde_json::json!({ "issuer": identity.issuer, "subject": identity.subject })).await?;
                tx.commit().await?;
                return Ok(SsoOutcome::Merged(id));
            }
        }
    }

    if !policy.enable_signup {
        return Ok(SsoOutcome::NoAccount);
    }
    let mut tx = pool.begin().await?;
    // An email someone else holds is simply not recorded: never a merge here.
    let email = match verified_email.as_deref() {
        Some(e) => {
            let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principal WHERE lower(email) = $1 AND status <> 'DISABLED')")
                .bind(e)
                .fetch_one(&mut *tx)
                .await?;
            (!taken).then_some(e)
        }
        None => None,
    };
    let role = if policy.pending_by_default { AccountRole::Pending } else { AccountRole::User };
    let acct = NewAccount { name: identity.display_name.as_deref(), email, password_hash: None, role };
    let actor = format!("sso:{}", identity.issuer);
    let (id, _) = match create_account(&mut tx, &acct, &actor, "signup.sso").await {
        Ok(r) => r,
        Err(AccountError::Db(e)) => return Err(e),
        Err(_) => return Ok(SsoOutcome::NoAccount),
    };
    link_identity(&mut tx, identity, id).await?;
    tx.commit().await?;
    Ok(SsoOutcome::Created(id))
}

async fn link_identity(tx: &mut Transaction<'_, Postgres>, identity: &VerifiedIdentity, principal_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO principal_identity (issuer, subject, principal_id, last_login_at) VALUES ($1, $2, $3, now())")
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .bind(principal_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    action: &str,
    target: Uuid,
    detail: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO audit_event (actor, action, target_principal_id, detail) VALUES ($1, $2, $3, $4)")
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
pub(crate) async fn generate_unique_code(tx: &mut Transaction<'_, Postgres>, seed: &str) -> Result<String, sqlx::Error> {
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

    #[test]
    fn slugs_and_emails() {
        assert_eq!(slugify("Ada.Lovelace+x"), "ada-lovelace-x");
        assert_eq!(normalise_email(" Ada@Example.COM ").unwrap(), "ada@example.com");
        assert!(normalise_email("ada").is_err());
    }
}
