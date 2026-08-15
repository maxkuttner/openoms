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

/// Create everything from scratch. Fails if any of it already exists.
///
/// Strict on purpose. A silent no-op hides the most common real mistake — being
/// pointed at the wrong server — and an automatic `ALTER ROLE … PASSWORD` would
/// let a bare `init` reset a working install's credentials to the shipped
/// default. `migrate` is the verb for a database that already exists.
pub async fn init(
    o: PostgresOverrides,
    mdm: Option<String>,
    oms: Option<String>,
    fixtures: bool,
) -> Fallible {
    let cfg = config::resolve(o);
    let roles = config::resolve_roles(mdm, oms);

    let existing = provision::inspect(&cfg).await?;
    if !existing.is_empty() {
        return Err(already_initialized(&cfg, &existing).into());
    }

    provision::provision(&cfg, &roles).await?;
    println!("  created roles mdm_master, oms_user");
    println!("  created database {}", cfg.database);

    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::apply_all(&pool).await?;
    println!("  applied {applied} migrations");

    access::apply(&pool).await?;
    println!("  applied grants");

    let venues = seed::seed_reference_data(&pool).await?;
    println!("  seeded reference data ({venues} venues)");

    if fixtures {
        seed::load_fixtures(&pool).await?;
        println!("  loaded dev fixtures (test-trader-key : test-secret)");
    }

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
pub async fn drop(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    provision::drop_database(&cfg).await?;
    println!("dropped database {} (roles kept)", cfg.database);
    Ok(())
}

/// What exists and what is outstanding.
pub async fn status(o: PostgresOverrides) -> Fallible {
    let cfg = config::resolve(o);
    let existing = provision::inspect(&cfg).await?;
    println!("server:   {}:{}", cfg.host, cfg.port);
    println!("database: {} ({})", cfg.database, if existing.database { "present" } else { "absent" });
    println!("roles:    {}", if existing.roles.is_empty() { "none".into() } else { existing.roles.join(", ") });

    if !existing.database {
        println!("\nNot initialized. Run: oms database init");
        return Ok(());
    }

    let pool = PgPool::connect(&cfg.url()).await?;
    migrate::ensure_tracking(&pool).await?;
    let applied = migrate::applied_count(&pool).await?;
    let pending = migrate::pending(&pool).await?;
    println!("migrations: {applied} applied, {} pending", pending.len());
    for p in &pending {
        println!("  pending  [{}] {}", p.schema, p.filename);
    }
    Ok(())
}

/// The strict-init error. Names what was found and what to run instead, because
/// "already exists" without a next step is the least useful thing this could say.
fn already_initialized(cfg: &config::PostgresConfig, existing: &provision::Existing) -> String {
    let mut msg = String::from("error: this server is already initialized\n");
    if !existing.roles.is_empty() {
        msg.push_str(&format!(
            "       role(s) {} already exist on {}:{}\n",
            existing.roles.join(", "), cfg.host, cfg.port
        ));
    }
    if existing.database {
        msg.push_str(&format!("       database '{}' already exists\n", cfg.database));
    }
    msg.push_str(
        "\n       Did you mean:\n\
         \x20        oms database migrate    apply pending migrations\n\
         \x20        oms database status     show what is there\n\
         \x20        oms database drop       destroy it and start over\n\
         \n       If you meant a different server, check POSTGRES_HOST / --host.",
    );
    msg
}

