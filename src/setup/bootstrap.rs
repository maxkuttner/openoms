//! Runtime setup that runs after the pool connects, as the `oms` role.
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

/// Ensure a `broker_connection` exists for every broker in `Broker::ALL`.
///
/// Routing config, not a fixture — but unlike the env-based world this used to
/// run in, a connection row is no longer evidence that credentials are present;
/// it is the target a credential attaches to (`oms config import-env`, and
/// eventually the cockpit, both write into an existing row rather than creating
/// one — see `import_env::run`). So this now runs unconditionally: an
/// uncredentialed row simply reads as "needs setup", exactly like a feed row
/// does (see [`ensure_feed_connections`]). Runs as the `oms` role (which owns
/// the schema), idempotent on `code`.
pub async fn ensure_broker_connections(pool: &PgPool) {
    for broker in Broker::ALL.iter().copied() {
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

/// Ensure a `feed_connection` row exists for the market-data feed this build
/// supports.
///
/// The exact counterpart to [`ensure_broker_connections`], and for the same
/// reason: a connection row is the target a credential attaches to, not
/// evidence that one exists. Without this, a fresh install had no feed row at
/// all — the cockpit's Data Feeds page showed "no feed connections", every
/// credential endpoint 404'd, and the only way to create the row was
/// `oms config import-env` with the key already in `.env`, which is the exact
/// workflow configuring feeds from the GUI exists to replace.
///
/// One row, hardcoded: `databento-opra` is the only feed this build ever
/// registers (`admin::classify_feed` and `serve()` both guard on that literal,
/// and `reload::restart_databento_feed` re-inserts under it). `provider` must
/// be `DATABENTO` to match what `credentials::save_feed` writes, so a row
/// seeded here and a row written by an import are indistinguishable. `dataset`
/// mirrors `credentials_api::DATABENTO_OPRA_DATASET`.
///
/// Runs as the `oms` role, idempotent on `code`, and deliberately does not
/// touch `credentials` — `DO NOTHING` so re-running at every boot can never
/// disturb a configured feed.
pub async fn ensure_feed_connections(pool: &PgPool) {
    const FEEDS: [(&str, &str, &str); 1] = [("databento-opra", "DATABENTO", "OPRA.PILLAR")];
    for (code, provider, dataset) in FEEDS {
        let res = sqlx::query(
            "INSERT INTO oms.feed_connection (code, provider, dataset, status) \
             VALUES ($1, $2, $3, 'ACTIVE') ON CONFLICT (code) DO NOTHING",
        )
        .bind(code)
        .bind(provider)
        .bind(dataset)
        .execute(pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() > 0 => info!("bootstrap: created feed_connection {code}"),
            Ok(_) => {} // already existed
            Err(e) => error!("bootstrap: could not ensure feed_connection {code}: {e}"),
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

/// Whether a boot-time sync will run: enabled, catalog empty, and `synced_brokers`
/// (the store-credentialed subset `serve()` already computed via
/// `brokers::brokers_with_creds`, so this and [`spawn_sync`] can never disagree
/// about which brokers are eligible) is non-empty. The caller uses this to tell
/// preflight an empty catalog is expected.
pub async fn will_sync_on_boot(pool: &PgPool, synced_brokers: &[Broker]) -> bool {
    sync_on_boot_enabled() && !synced_brokers.is_empty() && !catalog_nonempty(pool).await.unwrap_or(true)
}

/// Populate the instrument catalog from every broker in `pending` (the
/// store-credentialed set from [`will_sync_on_boot`]), on a dedicated background
/// thread.
///
/// A separate thread with its own current-thread runtime, not `tokio::spawn`: the
/// sync path threads a non-`Send` boxed error across its awaits, and this keeps that
/// contained instead of forcing `Send + Sync` through every adapter's error type.
/// The job is genuinely independent — `setup::brokers::run` opens its own pool as
/// the `oms` role — so nothing is shared with the server runtime. Caller checks
/// [`will_sync_on_boot`] first.
pub fn spawn_sync(pending: Vec<Broker>) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("bootstrap: could not start sync runtime: {e}");
                return;
            }
        };
        rt.block_on(sync_all_brokers(pending));
    });
}

/// Run `sync-broker` for each broker in `pending`. Each is independent: one being
/// unreachable logs and does not stop the others, nor the server.
///
/// `pending` reflects the store's view of who is credentialed (from
/// [`will_sync_on_boot`]); `brokers::run` sources its own credential from the same
/// store, so there is nothing to re-filter here — a broker in `pending` is
/// guaranteed to have a `Configured` credential `run` can read.
async fn sync_all_brokers(pending: Vec<Broker>) {
    let underlyings = env::var("OMS_SYNC_UNDERLYINGS").unwrap_or_default();
    info!("bootstrap: catalog empty — syncing {} broker(s) in background", pending.len());

    for broker in pending {
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

    // `Broker::has_env_creds` and its test (`alpaca_env_creds_detected_only_when_both_present`)
    // were removed along with the function: `setup::brokers::run` now sources
    // credentials from the store like everything else, so there is no more
    // env-reading cred-detection path for this module to test. `has_creds`, the
    // store-based check, is covered in `setup::brokers::tests` instead.

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
