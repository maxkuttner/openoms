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
//!
//! A login that arrives with a valid invite but without that proof (no
//! trustworthy `email_verified`, `trust_email_verified = false`, or a link
//! invite for an active user without that user's own session) becomes a
//! `pending_identity_binding`: it grants nothing, leaves the invite pending,
//! and waits for an admin to approve or reject the exact `(issuer, subject)`.
//! An admin may also pre-bind an `(issuer, subject)` directly.

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
    /// That `(issuer, subject)` is already linked to someone.
    IdentityTaken,
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
            InviteError::IdentityTaken => write!(f, "this sign-in identity is already linked"),
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
    /// The login was recorded as a pending binding for an admin to approve;
    /// nothing was granted and the invite stays pending.
    PendingApproval,
}

/// How much a login may prove on its own when claiming an invite.
#[derive(Debug, Clone, Copy)]
pub struct ClaimPolicy {
    /// `[auth.oidc].trust_email_verified`: a verified matching email claims an
    /// invite directly. Off, every invite claim waits for admin approval.
    pub trust_email_verified: bool,
    /// The principal behind a live session cookie on the browser presenting
    /// the callback, and whether that session passed MFA. Only that same
    /// principal, MFA-verified, can claim its own link invite.
    pub owner_session: Option<(Uuid, bool)>,
}

impl Default for ClaimPolicy {
    fn default() -> Self {
        ClaimPolicy { trust_email_verified: true, owner_session: None }
    }
}

/// An open request to bind a sign-in identity to an invited user.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingBinding {
    pub binding_id: Uuid,
    pub invite_id: Uuid,
    pub user_id: Uuid,
    pub user_code: String,
    pub invited_email: String,
    pub issuer: String,
    pub subject: String,
    /// What the provider claimed; not verified, for the admin to compare.
    pub untrusted_email: Option<String>,
    pub display_name: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
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
            ClaimOutcome::PendingApproval => Some("pending_approval"),
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
/// hash came with the login flow. A claim that lacks proof becomes a pending
/// binding (`PendingApproval`); every other refusal leaves the database
/// unchanged.
pub async fn resolve_login(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    invite_token_hash: Option<&str>,
    policy: &ClaimPolicy,
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
        let principal_id: Uuid = row.get("id");
        if kind == "HUMAN" && status == "INVITED" {
            // Pre-bound by an admin: the identity is proven, the invite is
            // still what lets the user in.
            return claim_prebound(pool, identity, principal_id, invite_token_hash).await;
        }
        // A linked identity never claims anything: a strategy or disabled
        // user behind it is simply refused.
        if kind != "HUMAN" || status != "ACTIVE" {
            return Ok(ClaimOutcome::NoInvite);
        }
        sqlx::query("UPDATE principal_identity SET last_login_at = now() WHERE issuer = $1 AND subject = $2")
            .bind(&identity.issuer)
            .bind(&identity.subject)
            .execute(pool)
            .await?;
        return Ok(ClaimOutcome::Known { principal_id, code: row.get("code") });
    }

    let Some(token_hash) = invite_token_hash else {
        return Ok(ClaimOutcome::NoInvite);
    };

    let mut tx = pool.begin().await?;
    let Some(invite) = lock_invite(&mut tx, token_hash).await? else {
        return Ok(ClaimOutcome::NoInvite);
    };
    if let Some(refusal) = invite.refusal() {
        return Ok(refusal);
    }

    let proven = if invite.status == "ACTIVE" {
        // A link invite adds a sign-in to an existing account: only that
        // account's own live session proves it, never the mailbox alone.
        policy.owner_session == Some((invite.principal_id, true))
    } else {
        let verified_email = identity.email.as_deref().filter(|_| identity.email_verified && policy.trust_email_verified);
        match verified_email {
            Some(email) if email.trim().eq_ignore_ascii_case(&invite.email) => true,
            Some(_) => return Ok(ClaimOutcome::EmailMismatch { invited_hint: mask_email(&invite.email) }),
            None => false,
        }
    };
    if !proven {
        open_binding(&mut tx, &invite, identity).await?;
        tx.commit().await?;
        return Ok(ClaimOutcome::PendingApproval);
    }

    link_identity(&mut tx, &invite, &identity.issuer, &identity.subject, identity.display_name.as_deref()).await?;
    close_open_bindings(&mut tx, invite.id, &identity.issuer, &identity.subject, &invite.code).await?;
    audit(
        &mut tx,
        &invite.code,
        "invite.accepted",
        invite.principal_id,
        serde_json::json!({ "invite_id": invite.id, "issuer": identity.issuer, "subject": identity.subject }),
    )
    .await?;
    tx.commit().await?;
    Ok(ClaimOutcome::Linked { principal_id: invite.principal_id, code: invite.code })
}

/// An invite row locked for a claim, with its user's code and status.
struct LockedInvite {
    id: Uuid,
    email: String,
    principal_id: Uuid,
    code: String,
    status: String,
    expires_at: DateTime<Utc>,
    accepted_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

impl LockedInvite {
    /// Why this invite cannot be claimed any more, if it cannot.
    fn refusal(&self) -> Option<ClaimOutcome> {
        if self.accepted_at.is_some() {
            Some(ClaimOutcome::NoInvite)
        } else if self.revoked_at.is_some() || self.status == "DISABLED" {
            Some(ClaimOutcome::Revoked)
        } else if self.expires_at <= Utc::now() {
            Some(ClaimOutcome::Expired)
        } else {
            None
        }
    }
}

const LOCK_INVITE: &str = "SELECT i.id, i.email, i.principal_id, i.expires_at, i.accepted_at, i.revoked_at, \
            p.code, p.status \
       FROM invite i JOIN principal p ON p.id = i.principal_id";

fn locked_invite(row: sqlx::postgres::PgRow) -> LockedInvite {
    LockedInvite {
        id: row.get("id"),
        email: row.get("email"),
        principal_id: row.get("principal_id"),
        code: row.get("code"),
        status: row.get("status"),
        expires_at: row.get("expires_at"),
        accepted_at: row.get("accepted_at"),
        revoked_at: row.get("revoked_at"),
    }
}

async fn lock_invite(tx: &mut Transaction<'_, Postgres>, token_hash: &str) -> Result<Option<LockedInvite>, sqlx::Error> {
    let row = sqlx::query(&format!("{LOCK_INVITE} WHERE i.token_hash = $1 FOR UPDATE OF i"))
        .bind(token_hash)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(locked_invite))
}

/// A pre-bound identity of an invited user claims that user's own invite.
async fn claim_prebound(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    principal_id: Uuid,
    invite_token_hash: Option<&str>,
) -> Result<ClaimOutcome, sqlx::Error> {
    let Some(token_hash) = invite_token_hash else {
        return Ok(ClaimOutcome::NoInvite);
    };
    let mut tx = pool.begin().await?;
    let Some(invite) = lock_invite(&mut tx, token_hash).await? else {
        return Ok(ClaimOutcome::NoInvite);
    };
    if invite.principal_id != principal_id {
        return Ok(ClaimOutcome::NoInvite);
    }
    if let Some(refusal) = invite.refusal() {
        return Ok(refusal);
    }
    activate(&mut tx, &invite, identity.display_name.as_deref()).await?;
    sqlx::query("UPDATE principal_identity SET last_login_at = now() WHERE issuer = $1 AND subject = $2")
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &invite.code,
        "invite.accepted",
        invite.principal_id,
        serde_json::json!({ "invite_id": invite.id, "issuer": identity.issuer, "subject": identity.subject, "prebound": true }),
    )
    .await?;
    tx.commit().await?;
    Ok(ClaimOutcome::Linked { principal_id, code: invite.code })
}

/// Records `identity` as waiting for approval on `invite`, expiring with it.
/// An identity already waiting (for this or another invite) is left as it is:
/// it has one open request at a time.
async fn open_binding(
    tx: &mut Transaction<'_, Postgres>,
    invite: &LockedInvite,
    identity: &VerifiedIdentity,
) -> Result<(), sqlx::Error> {
    // Provider/tenant context for the admin; never an authorization input.
    let metadata: serde_json::Map<String, serde_json::Value> = ["tid", "hd"]
        .iter()
        .filter_map(|k| identity.claims.get(*k).filter(|v| v.is_string()).map(|v| (k.to_string(), v.clone())))
        .collect();
    sqlx::query(
        "INSERT INTO pending_identity_binding \
            (id, invite_id, issuer, subject, untrusted_email, display_name, metadata, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (issuer, subject) WHERE decision IS NULL DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(invite.id)
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .bind(&identity.email)
    .bind(&identity.display_name)
    .bind(serde_json::Value::Object(metadata))
    .bind(invite.expires_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Links `(issuer, subject)` to the invite's user, activates the user and
/// consumes the invite.
async fn link_identity(
    tx: &mut Transaction<'_, Postgres>,
    invite: &LockedInvite,
    issuer: &str,
    subject: &str,
    display_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO principal_identity (issuer, subject, principal_id, last_login_at) \
         VALUES ($1, $2, $3, now())",
    )
    .bind(issuer)
    .bind(subject)
    .bind(invite.principal_id)
    .execute(&mut **tx)
    .await?;
    activate(tx, invite, display_name).await
}

async fn activate(
    tx: &mut Transaction<'_, Postgres>,
    invite: &LockedInvite,
    display_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE principal SET status = 'ACTIVE', \
                display_name = COALESCE(display_name, $2), updated_at = now() \
          WHERE id = $1",
    )
    .bind(invite.principal_id)
    .bind(display_name)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE invite SET accepted_at = now() WHERE id = $1")
        .bind(invite.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Once an invite is consumed (or an identity linked), every other open
/// request on that invite and for that identity is moot: close them so none
/// can be approved later.
async fn close_open_bindings(
    tx: &mut Transaction<'_, Postgres>,
    invite_id: Uuid,
    issuer: &str,
    subject: &str,
    actor: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE pending_identity_binding \
            SET decision = 'rejected', decided_at = now(), decided_by = $4 \
          WHERE decision IS NULL AND (invite_id = $1 OR (issuer = $2 AND subject = $3))",
    )
    .bind(invite_id)
    .bind(issuer)
    .bind(subject)
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Open, unexpired binding requests, oldest first.
pub async fn list_bindings(pool: &PgPool) -> Result<Vec<PendingBinding>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT b.id, b.invite_id, i.principal_id, p.code, i.email, b.issuer, b.subject, \
                b.untrusted_email, b.display_name, b.metadata, b.created_at, b.expires_at \
           FROM pending_identity_binding b \
           JOIN invite i ON i.id = b.invite_id \
           JOIN principal p ON p.id = i.principal_id \
          WHERE b.decision IS NULL AND b.expires_at > now() \
          ORDER BY b.created_at, b.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| PendingBinding {
            binding_id: r.get("id"),
            invite_id: r.get("invite_id"),
            user_id: r.get("principal_id"),
            user_code: r.get("code"),
            invited_email: r.get("email"),
            issuer: r.get("issuer"),
            subject: r.get("subject"),
            untrusted_email: r.get("untrusted_email"),
            display_name: r.get("display_name"),
            metadata: r.get("metadata"),
            created_at: r.get("created_at"),
            expires_at: r.get("expires_at"),
        })
        .collect())
}

/// Approve an open binding: in one transaction, link its `(issuer, subject)`
/// to the invited user, activate them, consume the invite, record the
/// decision and audit `binding.approved`. Returns the user's id.
pub async fn approve_binding(pool: &PgPool, binding_id: Uuid, actor: &str) -> Result<Uuid, InviteError> {
    let mut tx = pool.begin().await?;
    // Lock order is invite → binding, as on the claim path.
    let invite_id: Uuid = sqlx::query_scalar("SELECT invite_id FROM pending_identity_binding WHERE id = $1")
        .bind(binding_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InviteError::NotFound)?;
    let invite = sqlx::query(&format!("{LOCK_INVITE} WHERE i.id = $1 FOR UPDATE OF i"))
        .bind(invite_id)
        .fetch_one(&mut *tx)
        .await
        .map(locked_invite)?;
    let binding = sqlx::query(
        "SELECT issuer, subject, display_name FROM pending_identity_binding \
          WHERE id = $1 AND decision IS NULL AND expires_at > now() FOR UPDATE",
    )
    .bind(binding_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(InviteError::NotFound)?;
    let issuer: String = binding.get("issuer");
    let subject: String = binding.get("subject");
    let display_name: Option<String> = binding.get("display_name");
    if invite.refusal().is_some() {
        return Err(InviteError::NotFound);
    }
    if let Err(err) = link_identity(&mut tx, &invite, &issuer, &subject, display_name.as_deref()).await {
        return Err(if is_unique_violation(&err) { InviteError::IdentityTaken } else { err.into() });
    }
    sqlx::query(
        "UPDATE pending_identity_binding SET decision = 'approved', decided_at = now(), decided_by = $2 WHERE id = $1",
    )
    .bind(binding_id)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    close_open_bindings(&mut tx, invite.id, &issuer, &subject, actor).await?;
    audit(
        &mut tx,
        actor,
        "binding.approved",
        invite.principal_id,
        serde_json::json!({ "binding_id": binding_id, "invite_id": invite.id, "issuer": issuer, "subject": subject }),
    )
    .await?;
    tx.commit().await?;
    Ok(invite.principal_id)
}

/// Reject an open binding. The invite stays pending.
pub async fn reject_binding(pool: &PgPool, binding_id: Uuid, actor: &str) -> Result<(), InviteError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "UPDATE pending_identity_binding b \
            SET decision = 'rejected', decided_at = now(), decided_by = $2 \
           FROM invite i \
          WHERE b.id = $1 AND b.decision IS NULL AND b.expires_at > now() AND i.id = b.invite_id \
         RETURNING i.principal_id, b.issuer, b.subject",
    )
    .bind(binding_id)
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(InviteError::NotFound)?;
    let principal_id: Uuid = row.get("principal_id");
    let issuer: String = row.get("issuer");
    let subject: String = row.get("subject");
    audit(
        &mut tx,
        actor,
        "binding.rejected",
        principal_id,
        serde_json::json!({ "binding_id": binding_id, "issuer": issuer, "subject": subject }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// An admin binds an independently verified `(issuer, subject)` to a person
/// directly. Refused if the identity is already linked to anyone.
pub async fn prebind_identity(
    pool: &PgPool,
    principal_id: Uuid,
    issuer: &str,
    subject: &str,
    actor: &str,
) -> Result<(), InviteError> {
    let mut tx = pool.begin().await?;
    let person: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM principal WHERE id = $1 AND principal_type = 'HUMAN' AND status IN ('ACTIVE', 'INVITED') FOR UPDATE",
    )
    .bind(principal_id)
    .fetch_optional(&mut *tx)
    .await?;
    if person.is_none() {
        return Err(InviteError::NotFound);
    }
    let inserted = sqlx::query("INSERT INTO principal_identity (issuer, subject, principal_id) VALUES ($1, $2, $3)")
        .bind(issuer)
        .bind(subject)
        .bind(principal_id)
        .execute(&mut *tx)
        .await;
    if let Err(err) = inserted {
        return Err(if is_unique_violation(&err) { InviteError::IdentityTaken } else { err.into() });
    }
    // A pending request for this identity is settled by the admin's binding.
    sqlx::query(
        "UPDATE pending_identity_binding SET decision = 'rejected', decided_at = now(), decided_by = $3 \
          WHERE decision IS NULL AND issuer = $1 AND subject = $2",
    )
    .bind(issuer)
    .bind(subject)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "identity.prebound",
        principal_id,
        serde_json::json!({ "issuer": issuer, "subject": subject }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
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

    /// `resolve_login` as the callback calls it for a flow that carried
    /// `token`, under the default policy (trust `email_verified`, no session).
    async fn claim(pool: &PgPool, identity: &VerifiedIdentity, token: Option<&str>) -> Result<ClaimOutcome, sqlx::Error> {
        let hash = token.map(hash_session_token);
        resolve_login(pool, identity, hash.as_deref(), &ClaimPolicy::default()).await
    }

    async fn open_bindings(pool: &PgPool) -> Vec<(Uuid, Uuid, String)> {
        sqlx::query_as("SELECT id, invite_id, subject FROM pending_identity_binding WHERE decision IS NULL ORDER BY created_at")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn invite_accepted(pool: &PgPool, invite_id: Uuid) -> bool {
        sqlx::query_scalar("SELECT accepted_at IS NOT NULL FROM invite WHERE id = $1")
            .bind(invite_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn unverified_email_creates_pending_binding_not_session() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let mut identity = login(ISSUER, "entra-1", "jane@fundco.com", false);
        identity.claims = serde_json::json!({ "tid": "tenant-1", "upn": "jane@fundco.com" });

        let outcome = claim(&pool, &identity, Some(&invite.token)).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
        assert_eq!(outcome.refusal_reason(), Some("pending_approval"));

        let row = sqlx::query(
            "SELECT b.invite_id, b.issuer, b.subject, b.untrusted_email, b.display_name, b.metadata, \
                    b.expires_at = i.expires_at AS same_expiry \
               FROM pending_identity_binding b JOIN invite i ON i.id = b.invite_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<Uuid, _>("invite_id"), invite.invite_id);
        assert_eq!(row.get::<String, _>("issuer"), ISSUER);
        assert_eq!(row.get::<String, _>("subject"), "entra-1");
        assert_eq!(row.get::<Option<String>, _>("untrusted_email").as_deref(), Some("jane@fundco.com"));
        assert_eq!(row.get::<Option<String>, _>("display_name").as_deref(), Some("Jane"));
        assert_eq!(row.get::<serde_json::Value, _>("metadata"), serde_json::json!({ "tid": "tenant-1" }));
        assert!(row.get::<bool, _>("same_expiry"));

        // Nothing granted: no identity, no session, invite still pending.
        assert_eq!(count(&pool, "principal_identity").await, 0);
        assert_eq!(count(&pool, "user_session").await, 0);
        assert_eq!(status(&pool, invite.principal_id).await, "INVITED");
        assert!(!invite_accepted(&pool, invite.invite_id).await);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn pending_binding_grants_no_access_until_approved() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let identity = login(ISSUER, "entra-1", "jane@fundco.com", false);
        claim(&pool, &identity, Some(&invite.token)).await.unwrap();

        // Without the token the identity is a stranger; with it, still pending.
        assert_eq!(claim(&pool, &identity, None).await.unwrap(), ClaimOutcome::NoInvite);
        assert_eq!(claim(&pool, &identity, Some(&invite.token)).await.unwrap(), ClaimOutcome::PendingApproval);
        assert_eq!(open_bindings(&pool).await.len(), 1, "a repeat login does not stack bindings");
        assert_eq!(count(&pool, "principal_identity").await, 0);

        let (binding, ..) = open_bindings(&pool).await[0];
        approve_binding(&pool, binding, "ada").await.unwrap();
        assert_eq!(
            claim(&pool, &identity, None).await.unwrap(),
            ClaimOutcome::Known { principal_id: invite.principal_id, code: "jane".into() }
        );
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn approve_binding_links_and_activates_atomically() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "entra-1", "jane@fundco.com", false), Some(&invite.token)).await.unwrap();
        // A second identity tried the same leaked link.
        claim(&pool, &login(ISSUER, "mallory", "jane@fundco.com", false), Some(&invite.token)).await.unwrap();
        let open = open_bindings(&pool).await;
        assert_eq!(open.len(), 2);
        let listed = list_bindings(&pool).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].invited_email, "jane@fundco.com");
        let jane = open.iter().find(|b| b.2 == "entra-1").unwrap().0;

        let approved = approve_binding(&pool, jane, "ada").await.unwrap();
        assert_eq!(approved, invite.principal_id);
        assert_eq!(status(&pool, invite.principal_id).await, "ACTIVE");
        assert!(invite_accepted(&pool, invite.invite_id).await);
        let linked: Uuid = sqlx::query_scalar("SELECT principal_id FROM principal_identity WHERE issuer = $1 AND subject = 'entra-1'")
            .bind(ISSUER)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(linked, invite.principal_id);
        let (decision, by): (String, String) =
            sqlx::query_as("SELECT decision, decided_by FROM pending_identity_binding WHERE id = $1")
                .bind(jane)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((decision.as_str(), by.as_str()), ("approved", "ada"));
        let actor: String = sqlx::query_scalar(
            "SELECT actor FROM audit_event WHERE action = 'binding.approved' AND target_principal_id = $1",
        )
        .bind(invite.principal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(actor, "ada");
        // The rival request on the consumed invite is closed, not left to approve.
        assert!(open_bindings(&pool).await.is_empty());
        assert_eq!(count(&pool, "principal_identity").await, 1);
        // Approving twice is refused.
        assert!(matches!(approve_binding(&pool, jane, "ada").await, Err(InviteError::NotFound)));

        // Atomic: an approval that fails part-way leaves nothing behind.
        let other = create(&pool, "bob@fundco.com", "viewer", &[], "admin").await.unwrap();
        claim(&pool, &login(ISSUER, "bob-1", "bob@fundco.com", false), Some(&other.token)).await.unwrap();
        let bob = open_bindings(&pool).await[0].0;
        // Someone pre-bound the same identity meanwhile: the insert conflicts.
        sqlx::query("INSERT INTO principal_identity (issuer, subject, principal_id) VALUES ($1, 'bob-1', $2)")
            .bind(ISSUER)
            .bind(invite.principal_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(approve_binding(&pool, bob, "ada").await, Err(InviteError::IdentityTaken)));
        assert_eq!(status(&pool, other.principal_id).await, "INVITED");
        assert!(!invite_accepted(&pool, other.invite_id).await);
        assert_eq!(open_bindings(&pool).await.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn reject_binding_keeps_invite_pending() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let identity = login(ISSUER, "entra-1", "jane@fundco.com", false);
        claim(&pool, &identity, Some(&invite.token)).await.unwrap();
        let (binding, ..) = open_bindings(&pool).await[0];

        reject_binding(&pool, binding, "ada").await.unwrap();
        assert!(open_bindings(&pool).await.is_empty());
        assert!(!invite_accepted(&pool, invite.invite_id).await);
        assert_eq!(status(&pool, invite.principal_id).await, "INVITED");
        assert_eq!(count(&pool, "principal_identity").await, 0);
        let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'binding.rejected' AND actor = 'ada'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(audited, 1);
        assert!(matches!(reject_binding(&pool, binding, "ada").await, Err(InviteError::NotFound)));
        assert!(matches!(approve_binding(&pool, binding, "ada").await, Err(InviteError::NotFound)));

        // The invite is still claimable by a verified login.
        let outcome = claim(&pool, &login(ISSUER, "s2", "jane@fundco.com", true), Some(&invite.token)).await.unwrap();
        assert!(matches!(outcome, ClaimOutcome::Linked { .. }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn second_open_binding_for_same_identity_refused() {
        let pool = migrated_db().await;
        let jane = invite_jane(&pool).await;
        let bob = create(&pool, "bob@fundco.com", "viewer", &[], "admin").await.unwrap();
        let identity = login(ISSUER, "entra-1", "jane@fundco.com", false);
        claim(&pool, &identity, Some(&jane.token)).await.unwrap();

        // The same identity cannot queue for a second invite meanwhile.
        assert_eq!(claim(&pool, &identity, Some(&bob.token)).await.unwrap(), ClaimOutcome::PendingApproval);
        let open = open_bindings(&pool).await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].1, jane.invite_id);

        // The schema enforces it too.
        let dup = sqlx::query(
            "INSERT INTO pending_identity_binding (id, invite_id, issuer, subject, expires_at) \
             VALUES ($1, $2, $3, 'entra-1', now() + interval '1 day')",
        )
        .bind(Uuid::new_v4())
        .bind(bob.invite_id)
        .bind(ISSUER)
        .execute(&pool)
        .await;
        assert!(dup.is_err());

        // A decision must be complete: decision, decided_at and decided_by together.
        let partial = sqlx::query("UPDATE pending_identity_binding SET decision = 'rejected' WHERE id = $1")
            .bind(open[0].0)
            .execute(&pool)
            .await;
        assert!(partial.is_err());
        // The invite cannot be deleted out from under its approval record.
        let deleted = sqlx::query("DELETE FROM invite WHERE id = $1").bind(jane.invite_id).execute(&pool).await;
        assert!(deleted.is_err());
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn trust_email_verified_false_forces_approval() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let policy = ClaimPolicy { trust_email_verified: false, owner_session: None };
        let identity = login(ISSUER, "s1", "jane@fundco.com", true);

        let outcome = resolve_login(&pool, &identity, Some(&hash_session_token(&invite.token)), &policy).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
        assert_eq!(count(&pool, "principal_identity").await, 0);
        assert_eq!(open_bindings(&pool).await.len(), 1);
        assert!(!invite_accepted(&pool, invite.invite_id).await);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn link_invite_requires_owner_session() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token)).await.unwrap();
        let link = create_link(&pool, invite.principal_id, "admin").await.unwrap();
        let hash = hash_session_token(&link.token);
        let github = login("https://github.test", "gh-1", "jane@fundco.com", true);

        // Mailbox possession plus a verified email is not enough.
        let outcome = resolve_login(&pool, &github, Some(&hash), &ClaimPolicy::default()).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
        // Nor is someone else's session.
        let stranger = ClaimPolicy { trust_email_verified: true, owner_session: Some((Uuid::new_v4(), true)) };
        let outcome = resolve_login(&pool, &github, Some(&hash), &stranger).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
        assert_eq!(count(&pool, "principal_identity").await, 1);
        assert!(!invite_accepted(&pool, link.invite_id).await);

        // The owner's own session links it.
        let owner = ClaimPolicy { trust_email_verified: true, owner_session: Some((invite.principal_id, true)) };
        let outcome = resolve_login(&pool, &github, Some(&hash), &owner).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::Linked { principal_id: invite.principal_id, code: "jane".into() });
        assert_eq!(count(&pool, "principal_identity").await, 2);
        assert!(open_bindings(&pool).await.is_empty(), "the linked identity's pending request is closed");
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn link_invite_with_non_mfa_owner_session_goes_pending() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token)).await.unwrap();
        let link = create_link(&pool, invite.principal_id, "admin").await.unwrap();
        let github = login("https://github.test", "gh-1", "jane@fundco.com", true);

        let no_mfa = ClaimPolicy { trust_email_verified: true, owner_session: Some((invite.principal_id, false)) };
        let outcome = resolve_login(&pool, &github, Some(&hash_session_token(&link.token)), &no_mfa).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
        assert_eq!(count(&pool, "principal_identity").await, 1);
        assert_eq!(open_bindings(&pool).await.len(), 1);
        assert!(!invite_accepted(&pool, link.invite_id).await);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn expired_binding_cannot_be_rejected() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "entra-1", "jane@fundco.com", false), Some(&invite.token)).await.unwrap();
        let (binding, ..) = open_bindings(&pool).await[0];
        sqlx::query("UPDATE pending_identity_binding SET expires_at = now() - interval '1 minute' WHERE id = $1")
            .bind(binding)
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(reject_binding(&pool, binding, "ada").await, Err(InviteError::NotFound)));
        assert!(matches!(approve_binding(&pool, binding, "ada").await, Err(InviteError::NotFound)));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn prebind_identity_then_login_resolves() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let entra = login(ISSUER, "entra-oid-1", "whatever@else.com", false);

        prebind_identity(&pool, invite.principal_id, ISSUER, "entra-oid-1", "ada").await.unwrap();
        assert!(matches!(
            prebind_identity(&pool, invite.principal_id, ISSUER, "entra-oid-1", "ada").await,
            Err(InviteError::IdentityTaken)
        ));
        let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'identity.prebound' AND actor = 'ada'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(audited, 1);

        // Still invited: the pre-bound identity gets in only with the invite.
        assert_eq!(claim(&pool, &entra, None).await.unwrap(), ClaimOutcome::NoInvite);
        let outcome = claim(&pool, &entra, Some(&invite.token)).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::Linked { principal_id: invite.principal_id, code: "jane".into() });
        assert_eq!(status(&pool, invite.principal_id).await, "ACTIVE");
        assert!(invite_accepted(&pool, invite.invite_id).await);
        assert_eq!(claim(&pool, &entra, None).await.unwrap(), ClaimOutcome::Known { principal_id: invite.principal_id, code: "jane".into() });

        // Another user's invite cannot be claimed through a pre-bound identity.
        let bob = create(&pool, "bob@fundco.com", "viewer", &[], "admin").await.unwrap();
        prebind_identity(&pool, bob.principal_id, ISSUER, "bob-oid", "ada").await.unwrap();
        let carol = create(&pool, "carol@fundco.com", "viewer", &[], "admin").await.unwrap();
        let bob_login = login(ISSUER, "bob-oid", "bob@fundco.com", true);
        assert_eq!(claim(&pool, &bob_login, Some(&carol.token)).await.unwrap(), ClaimOutcome::NoInvite);
        assert_eq!(status(&pool, carol.principal_id).await, "INVITED");
        // An identity already linked elsewhere cannot be pre-bound.
        assert!(matches!(
            prebind_identity(&pool, carol.principal_id, ISSUER, "bob-oid", "ada").await,
            Err(InviteError::IdentityTaken)
        ));
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

        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
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

        let outcome = claim(&pool, &login(ISSUER, "stranger", "x@y.com", true), None).await.unwrap();
        assert_eq!(outcome, ClaimOutcome::NoInvite);
        let outcome = claim(&pool, &login(ISSUER, "stranger", "x@y.com", true), Some("forged"))
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

        let outcome = claim(&pool, &identity, Some(&invite.token)).await.unwrap();
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
        let again = claim(&pool, &identity, None).await.unwrap();
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

        let outcome = claim(&pool, &login(ISSUER, "s1", "bob@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::EmailMismatch { invited_hint: "j…@fundco.com".into() });
        assert_eq!(status(&pool, invite.principal_id).await, "INVITED");
        assert_eq!(count(&pool, "principal_identity").await, 0);

        // Still claimable by the right address.
        let outcome = claim(&pool, &login(ISSUER, "s2", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Linked { .. }));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn email_compare_is_case_insensitive() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let outcome = claim(
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
    async fn unverified_email_is_not_claimed() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        let outcome = claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", false), Some(&invite.token))
            .await
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::PendingApproval);
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
        let outcome = claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
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

        let outcome = claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
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
        assert_eq!(claim(&pool, &identity, Some(&old.token)).await.unwrap(), ClaimOutcome::Revoked);
        assert!(matches!(
            claim(&pool, &identity, Some(&new.token)).await.unwrap(),
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
        claim(&pool, &identity, Some(&third.token)).await.unwrap();
        assert!(matches!(reissue(&pool, third.invite_id, "admin").await, Err(InviteError::NotFound)));
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn second_provider_same_email_is_not_merged() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();

        let other = login("https://github.test", "gh-1", "jane@fundco.com", true);
        assert_eq!(claim(&pool, &other, None).await.unwrap(), ClaimOutcome::NoInvite);
        assert_eq!(count(&pool, "principal_identity").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires disposable Postgres via OMS_RELOAD_TEST_DATABASE_URL"]
    async fn invite_token_is_single_use() {
        let pool = migrated_db().await;
        let invite = invite_jane(&pool).await;
        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();
        let second = login("https://github.test", "gh-1", "jane@fundco.com", true);
        assert_eq!(claim(&pool, &second, Some(&invite.token)).await.unwrap(), ClaimOutcome::NoInvite);
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
        claim(&pool, &login(ISSUER, "s1", "jane@fundco.com", true), Some(&invite.token))
            .await
            .unwrap();

        let link = create_link(&pool, invite.principal_id, "admin").await.unwrap();
        assert_eq!(link.principal_id, invite.principal_id);
        let github = login("https://github.test", "gh-1", "jane@fundco.com", true);
        let owner = ClaimPolicy { trust_email_verified: true, owner_session: Some((invite.principal_id, true)) };
        let outcome = resolve_login(&pool, &github, Some(&hash_session_token(&link.token)), &owner).await.unwrap();
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
