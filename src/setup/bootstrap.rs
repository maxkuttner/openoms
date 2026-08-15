//! Runtime setup that runs after the pool connects, as `oms_user`.
//!
//! Provisioning moved to `oms database init` (see `setup::database`), so nothing
//! here touches admin credentials or creates schema. What remains is
//! environment-dependent configuration that only makes sense once the process is
//! actually running: a `broker_connection` row for each credentialed broker, and
//! the background instrument sync.

use std::env;

use sqlx::PgPool;
use tracing::{error, info};

use crate::setup::brokers::{self, Broker};

async fn catalog_nonempty(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument").fetch_one(pool).await?;
    Ok(n > 0)
}

/// Ensure a `broker_connection` exists for every broker whose creds are present.
///
/// This is routing config, not a fixture: a credentialed broker with no connection
/// row cannot take an order at all. Runs as `oms_user` (the `oms` schema is its own),
/// idempotent on `code`.
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
    fn sync_on_boot_defaults_on_and_only_never_disables() {
        env::set_var("OMS_SYNC_ON_BOOT", "never");
        assert!(!sync_on_boot_enabled());
        env::set_var("OMS_SYNC_ON_BOOT", "if-empty");
        assert!(sync_on_boot_enabled());
        env::remove_var("OMS_SYNC_ON_BOOT");
        assert!(sync_on_boot_enabled());
    }
}
