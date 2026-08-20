//! `oms database` — provisioning, migration and teardown.
//!
//! One file per phase, each replacing the shell script it was ported from. The
//! binary carries its own SQL (see `assets`), so a released `oms` provisions a
//! database without a repo checkout — which is what removes `psql` and `python3`
//! from the install requirements.

pub mod access;
pub mod assets;
pub mod config;
pub mod migrate;
pub mod provision;
pub mod seed;

use sqlx::PgPool;

use config::PostgresOverrides;

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// Create everything from scratch. Fails if any of it already exists, unless
/// `resume` is set.
///
/// Strict on purpose. A silent no-op hides the most common real mistake — being
/// pointed at the wrong server — and an automatic `ALTER ROLE … PASSWORD` would
/// let a bare `init` reset a working install's credentials to the shipped
/// default. `migrate` is the verb for a database that already exists.
///
/// `resume` exists for the failure path `oms init` cannot otherwise recover
/// from: once provisioning has created the role and/or database, a plain
/// `init` refuses (this function, strict by default) and `migrate` alone
/// leaves grants/seeding undone. With `resume` set, a role/database that
/// already exists is *not* an error — provisioning is skipped and the run
/// continues straight into the steps below. That is safe because every one of
/// those steps already tolerates being re-applied: migrations are tracked by
/// filename (`ensure_tracking`/`apply_all` skip anything already recorded),
/// `db/access/*.sql` are plain `GRANT`s (re-granting an existing privilege is a
/// no-op), and the seed data is upserted with `ON CONFLICT`. So resuming a
/// half-finished `init` is just running the idempotent tail again.
pub async fn init(o: PostgresOverrides, password: Option<String>, resume: bool) -> Fallible {
    let cfg = config::resolve(o);
    let password = config::resolve_role_password(password);

    // Same rule `serve()` enforces before connecting. `init` must check it too —
    // otherwise it happily *creates* the role with the default password on a remote
    // server, and the `serve()` guard only catches it after the fact.
    if cfg.refuses_default_password(&password) {
        return Err(format!(
            "error: refusing to create the {} role with the built-in default password on non-loopback host {}:{}\n\
             \x20        pass --oms-password, or set OMS_PASSWORD",
            provision::ROLE, cfg.host, cfg.port,
        )
        .into());
    }

    let existing = provision::inspect(&cfg).await?;
    match should_provision(resume, &existing) {
        Ok(true) => {
            provision::provision(&cfg, &password).await?;
            println!("  created role {}", provision::ROLE);
            println!("  created database {}", cfg.database);
        }
        Ok(false) => {
            println!("  --resume: role/database already present, skipping provisioning");
        }
        Err(()) => return Err(already_initialized(&cfg, &existing).into()),
    }

    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::apply_all(&pool).await?;
    println!("  applied {applied} migrations");

    access::apply(&pool).await?;
    println!("  applied grants");

    let venues = seed::seed_reference_data(&pool).await?;
    println!("  seeded reference data ({venues} venues)");

    println!("\n{} ready. Start the server with: cargo run", cfg.database);
    Ok(())
}

/// Apply pending migrations to a database that already exists. Idempotent.
pub async fn migrate(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::apply_all(&pool).await?;
    println!("applied {applied} migration(s)");
    Ok(())
}

/// Destroy the database. Roles survive — they are cluster-wide and may own
/// objects in other databases.
///
/// `yes` must be set to drop a non-loopback target: `DROP DATABASE … WITH
/// (FORCE)` terminates live sessions, so this needs the same friction a
/// destructive prod command always needs. Loopback stays frictionless — that is
/// the whole point of a local dev database.
pub async fn drop(o: PostgresOverrides, yes: bool) -> Fallible {
    let cfg = config::resolve(o);
    if !cfg.is_loopback() && !yes {
        return Err(format!(
            "error: refusing to drop database '{}' on {}:{} without --yes",
            cfg.database, cfg.host, cfg.port
        )
        .into());
    }
    provision::drop_database(&cfg).await?;
    println!("dropped database {} (roles kept)", cfg.database);
    Ok(())
}

/// What exists and what is outstanding.
pub async fn status(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);

    // `inspect` only reads pg_roles/pg_database — public catalogs any authenticated
    // role can see — but *connecting* still needs a real credential, and the
    // superuser one is what this task is trying to stop demanding. Try the oms
    // role first, on the same maintenance database inspect always used; that
    // authentication is only possible once `init` has already created the role,
    // which is exactly the common case (a database that already exists and this
    // command is being asked to describe). The one case it cannot cover is "has
    // this server ever been initialized at all" — the oms role does not exist yet
    // to authenticate as, so that check still falls back to the superuser cfg.
    let role_cfg = config::PostgresConfig {
        username: provision::ROLE.into(),
        password: config::resolve_role_password(None),
        ..cfg.clone()
    };
    let existing = match provision::inspect(&role_cfg).await {
        Ok(existing) => existing,
        Err(_) => provision::inspect(&cfg).await?,
    };
    println!("server:   {}:{}", cfg.host, cfg.port);
    println!("database: {} ({})", cfg.database, if existing.database { "present" } else { "absent" });
    println!("role:     {}", if existing.roles.is_empty() { "absent".into() } else { existing.roles.join(", ") });

    if !existing.database {
        println!("\nNot initialized. Run: oms database init");
        return Ok(());
    }

    // Read-only inspection must never create anything — unlike `init`/`migrate`,
    // which both call `ensure_tracking`. Pointed at a stale POSTGRES_DATABASE,
    // `ensure_tracking` would silently plant an `oms` schema in someone else's
    // database; `status` just reports what it finds.
    // Connect as the ordinary role, not the superuser: `status` is a read-only
    // inspection, and requiring a database-dropping credential to ask "is this
    // migrated?" would mean prompting for it constantly. Migration 0024 grants
    // this role SELECT on the tracking table.
    //
    // Falls back to the superuser the same way `inspect` above does: an install
    // provisioned before this fallback existed may have an `oms` role password
    // that was only ever given via `--oms-password` at provision time and never
    // stored in the environment or `oms.toml` — nothing here can reconstruct it.
    // Without the fallback that install's `status` would die on a credential this
    // command was specifically changed to stop requiring in the common case.
    let role_password = config::resolve_role_password(None);
    let pool = match PgPool::connect(&cfg.runtime_url(&role_password)).await {
        Ok(pool) => pool,
        Err(role_err) => match PgPool::connect(&cfg.url()).await {
            Ok(pool) => pool,
            Err(super_err) => {
                return Err(format!(
                    "error: could not connect as the {} role ({role_err}) or as the superuser ({super_err})",
                    provision::ROLE,
                )
                .into());
            }
        },
    };
    if !migrate::is_migrated(&pool).await? {
        println!("\nnot migrated — run `oms database init`");
        return Ok(());
    }
    let applied = migrate::applied_count(&pool).await?;
    let pending = migrate::pending(&pool).await?;
    println!("migrations: {applied} applied, {} pending", pending.len());
    for p in &pending {
        println!("  pending  [{}] {}", p.schema, p.filename);
    }
    Ok(())
}

/// Whether `init` should attempt `provision::provision`, given what
/// `provision::inspect` found and whether `--resume` was passed.
///
/// Split out from `init` so this decision — the crux of the strict-vs-resume
/// behaviour — is unit-testable without a live Postgres; everything else in
/// `init` needs a real connection.
///
/// `Ok(true)` — nothing exists, provision normally (true regardless of `resume`:
/// a `--resume` against a fresh server is just a normal init).
/// `Ok(false)` — something exists and `resume` was passed: skip provisioning,
/// the caller proceeds straight to the idempotent tail.
/// `Err(())` — something exists and `resume` was not passed: strict refusal.
fn should_provision(resume: bool, existing: &provision::Existing) -> Result<bool, ()> {
    if existing.is_empty() {
        Ok(true)
    } else if resume {
        Ok(false)
    } else {
        Err(())
    }
}

/// The strict-init error. Names what was found and what to run instead, because
/// "already exists" without a next step is the least useful thing this could say.
fn already_initialized(cfg: &config::PostgresConfig, existing: &provision::Existing) -> String {
    let mut msg = String::from("error: this server is already initialized\n");
    if !existing.roles.is_empty() {
        msg.push_str(&format!(
            "       role '{}' already exists on {}:{}\n",
            existing.roles.join(", "), cfg.host, cfg.port
        ));
    }
    if existing.database {
        msg.push_str(&format!("       database '{}' already exists\n", cfg.database));
    }
    msg.push_str(
        "\n       Did you mean:\n\
         \x20        oms database migrate         apply pending migrations\n\
         \x20        oms database status          show what is there\n\
         \x20        oms database drop            destroy it and start over\n\
         \x20        oms database init --resume   finish an init that failed partway through\n\
         \n       If you meant a different server, check POSTGRES_HOST / --host.",
    );
    msg
}

#[cfg(test)]
mod init_tests {
    use super::*;

    /// A fresh server always provisions, whether or not `--resume` was passed —
    /// `--resume` only changes behaviour when there is something to resume from.
    #[test]
    fn fresh_server_always_provisions() {
        assert_eq!(should_provision(false, &provision::Existing::default()), Ok(true));
        assert_eq!(should_provision(true, &provision::Existing::default()), Ok(true));
    }

    /// The default (no `--resume`) is unchanged: anything existing is a hard
    /// refusal, never a silent skip.
    #[test]
    fn existing_without_resume_refuses() {
        let existing = provision::Existing { roles: vec!["oms".into()], database: true };
        assert_eq!(should_provision(false, &existing), Err(()));

        let role_only = provision::Existing { roles: vec!["oms".into()], database: false };
        assert_eq!(should_provision(false, &role_only), Err(()));

        let db_only = provision::Existing { roles: vec![], database: true };
        assert_eq!(should_provision(false, &db_only), Err(()));
    }

    /// `--resume` against a server with a role and/or database already present
    /// skips provisioning rather than erroring — this is what lets `init` recover
    /// from a failure after `provision::provision` succeeded.
    #[test]
    fn existing_with_resume_skips_provisioning() {
        let existing = provision::Existing { roles: vec!["oms".into()], database: true };
        assert_eq!(should_provision(true, &existing), Ok(false));

        let role_only = provision::Existing { roles: vec!["oms".into()], database: false };
        assert_eq!(should_provision(true, &role_only), Ok(false));

        let db_only = provision::Existing { roles: vec![], database: true };
        assert_eq!(should_provision(true, &db_only), Ok(false));
    }
}

