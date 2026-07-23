//! Boot-time self-provisioning, so a fresh checkout is `run the app` with no ordered
//! setup commands.
//!
//! Two phases, deliberately split by *when* they run and *which role* they use:
//!
//! * [`ensure_ready`] runs **before** the runtime pool connects, orchestrating the
//!   existing idempotent scripts (`provision` → `migrate` → `access` → `seed`, plus
//!   the SPY fixture on a no-creds install) as the **admin** role. The scripts use
//!   psql meta-commands (`\gexec`, `\i`, `SET ROLE`) that sqlx can't run, so we
//!   shell out rather than reimplement tested SQL.
//! * [`spawn_sync`] runs **after** the server is listening, as the ordinary
//!   `oms_user`, populating the instrument catalog from each broker whose creds are
//!   present. It is slow (Alpaca's option chain is minutes) so it never blocks boot.
//!
//! The admin phase is gated by `OMS_BOOTSTRAP` (`auto` default | `off`) and skips
//! itself when the provisioning creds are incomplete — that is the
//! externally-provisioned path, and it means bootstrap never turns a DB that already
//! boots into one that doesn't. The least-privilege posture is unchanged: the
//! *runtime* pool is still `oms_user`; only this bounded boot step touches admin
//! creds, which already live in `.env`.

use std::env;
use std::path::PathBuf;
use std::process::Stdio;

use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::setup::brokers::{self, Broker};

/// Where the `db/scripts/*.sh` orchestration scripts live. Default is relative to
/// the working directory (repo root, as `cargo run` uses); override for an unusual
/// layout.
fn scripts_dir() -> PathBuf {
    env::var("OMS_DB_SCRIPTS_DIR")
        .unwrap_or_else(|_| "db/scripts".to_string())
        .into()
}

/// Is admin-phase bootstrap enabled? `off` opts out entirely (infra managed
/// elsewhere); anything else — including unset — is `auto`.
fn bootstrap_enabled() -> bool {
    !env::var("OMS_BOOTSTRAP").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

/// The env vars the bootstrap scripts require (provision needs the role passwords;
/// migrate/access/seed need the admin login). If any is absent we skip bootstrap
/// entirely rather than fail — a DB provisioned earlier from a fuller `.env` must
/// still boot from a leaner one. Bootstrap only *adds* the "fresh install just
/// works" path; it must never make an already-working setup worse.
fn have_provision_creds() -> bool {
    let set = |k: &str| env::var(k).is_ok_and(|v| !v.is_empty());
    ["ADMIN_USER", "ADMIN_PASSWORD", "MDM_MASTER_PASSWORD", "OMS_USER_PASSWORD"]
        .iter()
        .all(|k| set(k))
}

/// Provision/migrate/seed the database if needed, before the runtime pool connects.
///
/// Returns `Err` only when a script actually fails — a disabled or credential-less
/// bootstrap is `Ok(())` (the caller then connects and lets preflight judge the DB).
pub async fn ensure_ready() -> Result<(), Box<dyn std::error::Error>> {
    if !bootstrap_enabled() {
        info!("bootstrap: OMS_BOOTSTRAP=off — skipping (assuming externally provisioned)");
        return Ok(());
    }
    if !have_provision_creds() {
        info!("bootstrap: provisioning creds incomplete — skipping (assuming externally provisioned)");
        return Ok(());
    }

    // Idempotent, ordered: each reads what the one before it wrote. Re-running a
    // fully-provisioned DB is a cheap sequence of no-ops.
    run_script("provision.sh", &[]).await?;
    run_script("migrate.sh", &["up"]).await?;
    run_script("access.sh", &[]).await?;
    run_script("seed.sh", &[]).await?;

    Ok(())
}

/// Load the SPY fixture when the catalog is empty and no broker can populate it, so
/// a pure no-creds install still has one tradeable instrument. Best-effort: a
/// failure here logs and is swallowed — it is a convenience, not a prerequisite.
pub async fn ensure_fixture_if_no_brokers(pool: &PgPool) {
    if !bootstrap_enabled() || !have_provision_creds() {
        return;
    }
    if Broker::ALL.iter().any(|b| b.has_creds()) {
        return; // a broker will populate the catalog via sync_instruments
    }
    if catalog_nonempty(pool).await.unwrap_or(true) {
        return; // already have instruments; nothing to seed
    }
    if let Err(e) = run_script("fixtures.sh", &[]).await {
        warn!("bootstrap: fixture load failed (non-fatal): {e}");
    }
}

/// Run one `db/scripts/<name>` script, streaming its output into the log with a
/// `bootstrap:` prefix. The child inherits our environment, so the admin/DB creds
/// loaded from `.env` reach the script without re-parsing it.
async fn run_script(name: &str, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let path = scripts_dir().join(name);
    info!("bootstrap: running {}", path.display());

    let mut child = Command::new("bash")
        .arg(&path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("bootstrap: could not start {}: {e}", path.display()))?;

    // Drain both pipes concurrently so a chatty script can't deadlock on a full one.
    if let Some(s) = child.stdout.take() {
        tokio::spawn(log_lines(name.to_string(), s, false));
    }
    if let Some(s) = child.stderr.take() {
        tokio::spawn(log_lines(name.to_string(), s, true));
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(format!(
            "bootstrap: {name} exited with {status}. Fix the DB/creds, or set \
             OMS_BOOTSTRAP=off if infrastructure is provisioned elsewhere."
        )
        .into());
    }
    Ok(())
}

/// Forward a child pipe to the log, one line at a time, tagged with the script name.
async fn log_lines<R>(script: String, reader: R, is_err: bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if is_err {
            warn!("bootstrap[{script}]: {line}");
        } else {
            info!("bootstrap[{script}]: {line}");
        }
    }
}

async fn catalog_nonempty(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument").fetch_one(pool).await?;
    Ok(n > 0)
}

/// Ensure a `broker_connection` exists for every broker whose creds are present.
///
/// This is routing config, not a fixture: a credentialed broker with no connection
/// row cannot take an order at all. Runs as `oms_user` (the `oms` schema is its own),
/// idempotent on `code`. Independent of `OMS_DEV_IDENTITY` — real routing must work
/// without the dev chain.
pub async fn ensure_broker_connections(pool: &PgPool) {
    for broker in Broker::ALL.iter().copied().filter(|b| b.has_creds()) {
        let code = broker.connection_code();
        let res = sqlx::query(
            "INSERT INTO oms.broker_connection (code, broker_code, environment, status) \
             VALUES ($1, $2, $3, 'ACTIVE') ON CONFLICT (code) DO NOTHING",
        )
        .bind(&code)
        .bind(broker.code())
        .bind(broker.environment())
        .execute(pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() > 0 => info!("bootstrap: created broker_connection {code}"),
            Ok(_) => {} // already existed
            Err(e) => error!("bootstrap: could not ensure broker_connection {code}: {e}"),
        }
    }
}

/// Is the dev identity chain enabled? Off unless `OMS_DEV_IDENTITY` is truthy — it
/// creates a known API secret and must never fire in a real deployment by default.
fn dev_identity_enabled() -> bool {
    env::var("OMS_DEV_IDENTITY")
        .is_ok_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Create the dev principal/account/portfolio/api-key so a fresh install can place a
/// paper order with no admin setup. Gated by `OMS_DEV_IDENTITY`; best-effort.
///
/// The SQL is compiled in (`include_str!`), so this needs no scripts on disk, and
/// runs as `oms_user` — every row it writes lives in the `oms` schema.
pub async fn ensure_dev_identity(pool: &PgPool) {
    if !dev_identity_enabled() {
        return;
    }
    const SQL: &str = include_str!("../../scripts/fixtures/dev_identity.sql");
    match sqlx::raw_sql(SQL).execute(pool).await {
        Ok(_) => info!("bootstrap: dev identity ready (test-trader-key : test-secret)"),
        Err(e) => warn!("bootstrap: dev identity setup failed (non-fatal): {e}"),
    }
}

/// True when the app should populate an empty catalog from brokers at boot.
///
/// `OMS_SYNC_ON_BOOT=never` opts out; anything else — including unset — is
/// `if-empty`.
pub fn sync_on_boot_enabled() -> bool {
    !env::var("OMS_SYNC_ON_BOOT").is_ok_and(|v| v.eq_ignore_ascii_case("never"))
}

/// Whether a boot-time sync will run: enabled, catalog empty, and some broker has
/// creds. The caller uses this to tell preflight an empty catalog is expected.
pub async fn will_sync_on_boot(pool: &PgPool) -> bool {
    sync_on_boot_enabled()
        && Broker::ALL.iter().any(|b| b.has_creds())
        && !catalog_nonempty(pool).await.unwrap_or(true)
}

/// Populate the instrument catalog from every broker whose creds are present, on a
/// dedicated background thread.
///
/// A separate thread with its own current-thread runtime, not `tokio::spawn`: the
/// sync path threads a non-`Send` boxed error across its awaits, and this keeps that
/// contained instead of forcing `Send + Sync` through every adapter's error type.
/// The job is genuinely independent — `setup::brokers::run` opens its own pool as
/// `oms_user` — so nothing is shared with the server runtime. Caller checks
/// [`will_sync_on_boot`] first.
pub fn spawn_sync() {
    std::thread::spawn(|| {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("bootstrap: could not start sync runtime: {e}");
                return;
            }
        };
        rt.block_on(sync_all_brokers());
    });
}

/// Run `sync-broker` for each broker whose creds are present. Each is independent:
/// one being unreachable logs and does not stop the others, nor the server.
async fn sync_all_brokers() {
    let underlyings = env::var("OMS_SYNC_UNDERLYINGS").unwrap_or_default();
    let brokers: Vec<Broker> = Broker::ALL.iter().copied().filter(|b| b.has_creds()).collect();
    info!("bootstrap: catalog empty — syncing {} broker(s) in background", brokers.len());

    for broker in brokers {
        let args = brokers::Args {
            broker,
            underlyings: underlyings.clone(),
            no_enrich: false,
            dry_run: false,
            allow_skips: true, // a partial catalog beats none; preflight still warns
        };
        match brokers::run(args).await {
            Ok(()) => info!("bootstrap: {} sync complete", broker.code()),
            Err(e) => error!("bootstrap: {} sync failed (server stays up): {e}", broker.code()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cred detection reads the `{BROKER}_{ENV}_*` vars for the active env. These
    /// tests mutate process env, so they must not run in parallel with anything else
    /// reading the same keys — serialized here by living in one test.
    #[test]
    fn alpaca_creds_detected_only_when_both_present() {
        env::set_var("ALPACA_ENV", "PAPER");
        env::remove_var("ALPACA_PAPER_API_KEY");
        env::remove_var("ALPACA_PAPER_API_SECRET");
        assert!(!Broker::Alpaca.has_creds());

        env::set_var("ALPACA_PAPER_API_KEY", "k");
        assert!(!Broker::Alpaca.has_creds(), "key alone is not enough");

        env::set_var("ALPACA_PAPER_API_SECRET", "s");
        assert!(Broker::Alpaca.has_creds());

        // An empty value is not a credential.
        env::set_var("ALPACA_PAPER_API_SECRET", "");
        assert!(!Broker::Alpaca.has_creds());

        env::remove_var("ALPACA_PAPER_API_KEY");
        env::remove_var("ALPACA_PAPER_API_SECRET");
    }

    #[test]
    fn bootstrap_off_is_the_only_disabling_value() {
        env::set_var("OMS_BOOTSTRAP", "off");
        assert!(!bootstrap_enabled());
        env::set_var("OMS_BOOTSTRAP", "OFF");
        assert!(!bootstrap_enabled(), "case-insensitive");
        env::set_var("OMS_BOOTSTRAP", "auto");
        assert!(bootstrap_enabled());
        env::remove_var("OMS_BOOTSTRAP");
        assert!(bootstrap_enabled(), "unset defaults to enabled");
    }

    #[test]
    fn sync_on_boot_defaults_on_and_only_never_disables() {
        env::set_var("OMS_SYNC_ON_BOOT", "never");
        assert!(!sync_on_boot_enabled());
        env::set_var("OMS_SYNC_ON_BOOT", "if-empty");
        assert!(sync_on_boot_enabled());
        env::remove_var("OMS_SYNC_ON_BOOT");
        assert!(sync_on_boot_enabled());
    }
}
