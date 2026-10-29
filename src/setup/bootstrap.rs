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

use crate::setup::brokers::Broker;

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

/// The credentialed brokers whose instrument universe was never loaded — no
/// `broker_instrument` row for their broker code — and so get a `system` universe
/// load at boot. Empty when `OMS_SYNC_ON_BOOT=never`.
///
/// Per broker, not "is the whole catalog empty": once Alpaca is loaded, a
/// Binance connection given credentials later must still load at the next boot.
/// A failed count is treated as "loaded" — never start a write on a guess.
pub async fn pending_boot_loads(pool: &PgPool, credentialed: &[Broker]) -> Vec<Broker> {
    if !sync_on_boot_enabled() {
        return Vec::new();
    }
    let mut pending = Vec::new();
    for &broker in credentialed {
        let n: Result<i64, _> = sqlx::query_scalar("SELECT count(*) FROM broker_instrument WHERE broker_code = $1")
            .bind(broker.code())
            .fetch_one(pool)
            .await;
        match n {
            Ok(0) => pending.push(broker),
            Ok(_) => {}
            Err(e) => error!("boot: could not count {} instruments, not loading: {e}", broker.code()),
        }
    }
    pending
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
