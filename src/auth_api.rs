//! Maps an authenticated OIDC subject onto a `principal` row.
//!
//! The OMS's existing identity for anything that can trade is `principal`; a
//! human who successfully completes OIDC login needs one too. This module
//! owns that mapping and, on first login, the provisioning of a new row.
//!
//! HTTP handlers for `/auth/*` are a later task's concern and live outside
//! this module — `resolve_or_provision` takes an already-`VerifiedIdentity`
//! and a database pool, and makes no assumption about how either arrived.

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::oidc::VerifiedIdentity;

/// The result of matching a verified identity to a principal.
pub enum ResolveOutcome {
    /// Matched an existing principal, or provisioned a new one.
    Resolved { principal_id: Uuid, principal_code: String },
    /// `required_claim` is configured and the identity's claims don't satisfy
    /// it. No principal was read, created, or touched.
    ClaimRejected,
    /// `external_subject` already belongs to a row this subject must never
    /// resolve onto or touch — a non-HUMAN principal (SERVICE, STRATEGY,
    /// DESK) or a DISABLED one. Nothing was created or modified: this is a
    /// refusal, not a provisioning opportunity, since `external_subject` is
    /// UNIQUE and this subject can never claim a different row.
    SubjectNotAvailable,
}

/// Match an authenticated subject to its principal, creating one on first sight.
///
/// A provisioned principal has no grants, and a principal without grants can do
/// nothing: `require_order_grant` gates every order path and `/portfolios`
/// returns only granted rows. So first login yields an identity that can see
/// nothing until an admin grants it a portfolio — which is why this is safe to
/// do automatically, and why it beats making an admin hand-copy opaque subject
/// strings before anyone can log in.
pub async fn resolve_or_provision(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    required_claim: Option<&(String, String)>,
) -> Result<ResolveOutcome, sqlx::Error> {
    // A SERVICE or STRATEGY principal must never hold a browser session, and a
    // DISABLED one must never resolve at all — both are enforced right here in
    // the match, not left to a caller to remember.
    let existing = sqlx::query(
        "SELECT id, code FROM principal \
         WHERE external_subject = $1 AND status = 'ACTIVE' AND principal_type = 'HUMAN'",
    )
    .bind(&identity.subject)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = existing {
        return Ok(ResolveOutcome::Resolved {
            principal_id: row.get("id"),
            principal_code: row.get("code"),
        });
    }

    // Checked before any insert: a rejected subject must leave no row behind.
    if let Some((claim_name, required_value)) = required_claim {
        if !claim_satisfies(&identity.claims, claim_name, required_value) {
            return Ok(ResolveOutcome::ClaimRejected);
        }
    }

    let code_seed = identity
        .email
        .as_deref()
        .and_then(|email| email.split('@').next())
        .filter(|local| !local.is_empty())
        .or(identity.display_name.as_deref())
        .unwrap_or(&identity.subject);
    let code = generate_unique_code(pool, code_seed).await?;

    // ON CONFLICT (external_subject) is what makes two concurrent first
    // logins for the same subject idempotent rather than a unique-violation
    // error: whichever insert loses the race just updates the winner's row
    // (display_name only — code, id, and grants are untouched) and returns
    // it, same as if it had matched on the lookup above.
    //
    // The DO UPDATE's WHERE guard is load-bearing: without it, a conflict
    // against a SERVICE/STRATEGY/DESK or DISABLED principal would still fire
    // the update and hand this subject that row's id, code, and every grant
    // it holds — a privilege escalation. With the guard, a conflict against
    // such a row satisfies no WHERE clause, so DO UPDATE affects zero rows
    // and RETURNING yields nothing: `fetch_optional` sees `None`, and that is
    // treated as a refusal below, not as "nothing happened, fall through to
    // provisioning" (this row already exists — provisioning would collide on
    // the UNIQUE external_subject too).
    let row = sqlx::query(
        "INSERT INTO principal (id, code, principal_type, external_subject, display_name, status) \
         VALUES ($1, $2, 'HUMAN', $3, $4, 'ACTIVE') \
         ON CONFLICT (external_subject) DO UPDATE SET display_name = EXCLUDED.display_name \
         WHERE principal.principal_type = 'HUMAN' AND principal.status = 'ACTIVE' \
         RETURNING id, code",
    )
    .bind(Uuid::new_v4())
    .bind(&code)
    .bind(&identity.subject)
    .bind(&identity.display_name)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(ResolveOutcome::SubjectNotAvailable);
    };

    Ok(ResolveOutcome::Resolved {
        principal_id: row.get("id"),
        principal_code: row.get("code"),
    })
}

/// True if `claims[claim_name]` equals `required_value`, whether the claim is
/// a bare string (`"groups": "traders"`) or an array of strings
/// (`"groups": ["traders", "staff"]`). Any other shape — absent, a number, an
/// object, an array of non-strings — never satisfies the gate.
fn claim_satisfies(claims: &serde_json::Value, claim_name: &str, required_value: &str) -> bool {
    match claims.get(claim_name) {
        Some(serde_json::Value::String(s)) => s == required_value,
        Some(serde_json::Value::Array(values)) => {
            values.iter().any(|v| v.as_str() == Some(required_value))
        }
        _ => false,
    }
}

/// Slugifies `seed` and, if that code is already taken, appends the lowest
/// free numeric suffix (`-2`, `-3`, ...). An admin granting portfolios should
/// see a human-readable name, not a UUID.
async fn generate_unique_code(pool: &PgPool, seed: &str) -> Result<String, sqlx::Error> {
    let base = slugify(seed);
    let base = if base.is_empty() { "principal".to_string() } else { base };

    let mut candidate = base.clone();
    let mut suffix = 1u32;
    loop {
        let taken: bool =
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM principal WHERE code = $1)")
                .bind(&candidate)
                .fetch_one(pool)
                .await?;
        if !taken {
            return Ok(candidate);
        }
        suffix += 1;
        candidate = format!("{base}-{suffix}");
    }
}

/// Lowercase alphanumerics joined by single hyphens; no leading, trailing, or
/// doubled hyphens.
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

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_first_login_provisions_a_human_with_no_grants() {
        let pool = test_pool().await;
        let identity = identity_for(&format!("sub-{}", Uuid::new_v4()));

        let outcome = resolve_or_provision(&pool, &identity, None).await.expect("resolve");

        let ResolveOutcome::Resolved { principal_id, .. } = outcome else {
            panic!("expected a principal");
        };
        let grants: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM principal_portfolio_grant WHERE principal_id = $1",
        )
        .bind(principal_id)
        .fetch_one(&pool)
        .await
        .expect("count grants");
        assert_eq!(grants, 0, "a new human must be able to do nothing until granted");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn logging_in_twice_does_not_create_a_second_principal() {
        let pool = test_pool().await;
        let identity = identity_for(&format!("sub-{}", Uuid::new_v4()));

        let first = resolve_or_provision(&pool, &identity, None).await.expect("first");
        let second = resolve_or_provision(&pool, &identity, None).await.expect("second");

        assert_eq!(principal_id_of(&first), principal_id_of(&second));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn an_existing_principal_is_matched_on_its_external_subject() {
        let pool = test_pool().await;
        let subject = format!("sub-{}", Uuid::new_v4());
        let expected = seed_principal_with_subject(&pool, &subject).await;

        let outcome = resolve_or_provision(&pool, &identity_for(&subject), None)
            .await
            .expect("resolve");

        assert_eq!(principal_id_of(&outcome), expected);
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_claim_gate_keeps_the_rest_of_the_tenant_out() {
        let pool = test_pool().await;
        let mut identity = identity_for(&format!("sub-{}", Uuid::new_v4()));
        identity.claims = serde_json::json!({ "groups": "everyone-else" });

        let gate = ("groups".to_string(), "traders".to_string());
        let outcome = resolve_or_provision(&pool, &identity, Some(&gate)).await.expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::ClaimRejected));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_claim_gate_lets_the_intended_slice_through() {
        let pool = test_pool().await;
        let mut identity = identity_for(&format!("sub-{}", Uuid::new_v4()));
        identity.claims = serde_json::json!({ "groups": ["traders", "staff"] });

        let gate = ("groups".to_string(), "traders".to_string());
        let outcome = resolve_or_provision(&pool, &identity, Some(&gate)).await.expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_subject_already_held_by_a_service_principal_is_refused_not_hijacked() {
        let pool = test_pool().await;
        let subject = format!("sub-{}", Uuid::new_v4());
        let (service_id, original_name) = seed_service_principal_with_subject(&pool, &subject).await;

        // An attacker-controlled display_name must not leak into the service
        // principal even though the ON CONFLICT arbiter (external_subject)
        // does match this row.
        let mut identity = identity_for(&subject);
        identity.display_name = Some("Attacker-Controlled Name".to_string());

        let outcome = resolve_or_provision(&pool, &identity, None).await.expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::SubjectNotAvailable));

        let principal_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM principal WHERE external_subject = $1")
                .bind(&subject)
                .fetch_one(&pool)
                .await
                .expect("count principals for subject");
        assert_eq!(principal_count, 1, "no second principal must be created for a taken subject");

        let name: String = sqlx::query_scalar("SELECT display_name FROM principal WHERE id = $1")
            .bind(service_id)
            .fetch_one(&pool)
            .await
            .expect("read display_name");
        assert_eq!(name, original_name, "the service principal must not be touched");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_subject_held_by_a_disabled_human_is_refused_not_reactivated() {
        let pool = test_pool().await;
        let subject = format!("sub-{}", Uuid::new_v4());
        let disabled_id = seed_disabled_human_with_subject(&pool, &subject).await;

        let outcome = resolve_or_provision(&pool, &identity_for(&subject), None)
            .await
            .expect("resolve");

        assert!(matches!(outcome, ResolveOutcome::SubjectNotAvailable));

        let status: String = sqlx::query_scalar("SELECT status FROM principal WHERE id = $1")
            .bind(disabled_id)
            .fetch_one(&pool)
            .await
            .expect("read status");
        assert_eq!(status, "DISABLED", "a disabled principal must not be reactivated by a login attempt");
    }

    // ── test plumbing ────────────────────────────────────────────────────────

    fn principal_id_of(outcome: &ResolveOutcome) -> Uuid {
        match outcome {
            ResolveOutcome::Resolved { principal_id, .. } => *principal_id,
            _ => panic!("expected a resolved principal"),
        }
    }

    /// A `VerifiedIdentity` carrying `subject` unchanged, with an email whose
    /// local part embeds `subject` so the code this test run provisions is
    /// unique without leaning on the collision-suffix path.
    fn identity_for(subject: &str) -> VerifiedIdentity {
        VerifiedIdentity {
            subject: subject.to_string(),
            display_name: Some("Test User".to_string()),
            email: Some(format!("{subject}@example.com")),
            claims: serde_json::json!({}),
        }
    }

    /// `main` loads .env before resolving config; a test binary does not, so
    /// without this the test resolves a different database than the server
    /// runs against. The `oms` role carries `search_path = oms, public`
    /// (db/access/roles.sql); these are the admin credentials, so set it here.
    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// Seeds an ACTIVE HUMAN principal already bound to `subject`, as if it
    /// had been provisioned by an earlier login (or created by an admin).
    /// Returns its id.
    async fn seed_principal_with_subject(pool: &sqlx::PgPool, subject: &str) -> Uuid {
        let id = Uuid::new_v4();
        let code = format!("seeded-{id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, external_subject, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $3, $2, 'ACTIVE')",
        )
        .bind(id)
        .bind(&code)
        .bind(subject)
        .execute(pool)
        .await
        .expect("seed principal");
        id
    }

    /// Seeds an ACTIVE SERVICE principal already bound to `subject` — the
    /// scenario where a human's `sub` collides with a machine credential's.
    /// Returns its id and the `display_name` it was seeded with, so callers
    /// can assert that name survives untouched.
    async fn seed_service_principal_with_subject(
        pool: &sqlx::PgPool,
        subject: &str,
    ) -> (Uuid, String) {
        let id = Uuid::new_v4();
        let code = format!("service-{id}");
        let display_name = format!("Service {id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, external_subject, display_name, status) \
             VALUES ($1, $2, 'SERVICE', $3, $4, 'ACTIVE')",
        )
        .bind(id)
        .bind(&code)
        .bind(subject)
        .bind(&display_name)
        .execute(pool)
        .await
        .expect("seed service principal");
        (id, display_name)
    }

    /// Seeds a DISABLED HUMAN principal already bound to `subject` — e.g. an
    /// offboarded user whose `external_subject` an admin never cleared.
    /// Returns its id.
    async fn seed_disabled_human_with_subject(pool: &sqlx::PgPool, subject: &str) -> Uuid {
        let id = Uuid::new_v4();
        let code = format!("disabled-{id}");
        sqlx::query(
            "INSERT INTO principal (id, code, principal_type, external_subject, display_name, status) \
             VALUES ($1, $2, 'HUMAN', $3, $2, 'DISABLED')",
        )
        .bind(id)
        .bind(&code)
        .bind(subject)
        .execute(pool)
        .await
        .expect("seed disabled principal");
        id
    }
}
