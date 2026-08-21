//! `oms config import-env` — the one-shot bridge off environment credentials.
//!
//! Reads the `{BROKER}_{ENV}_*` variables the OMS used to consult at boot, seals
//! them into the store, and reports what it took. Run once; after that the
//! environment is no longer consulted for credentials at all.
//!
//! Deliberately skips anything half-configured. A key with no secret is a typo,
//! and importing it would store a credential that cannot authenticate — which
//! then looks like a broker outage rather than a missing value.

use sqlx::PgPool;

use crate::credentials::{save_broker, save_feed, BrokerCredentials, FeedCredentials};
use crate::secrets::MasterKey;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Broker credentials found in the environment, keyed by `broker_connection.code`.
pub fn scan_env() -> Vec<(String, BrokerCredentials)> {
    let mut out = Vec::new();

    for env_name in ["PAPER", "LIVE"] {
        if let (Some(key), Some(secret)) = (
            var(&format!("ALPACA_{env_name}_API_KEY")),
            var(&format!("ALPACA_{env_name}_API_SECRET")),
        ) {
            out.push((
                format!("alpaca-{}", env_name.to_lowercase()),
                BrokerCredentials::Alpaca { key, secret },
            ));
        }

        let p = format!("IBKR_{env_name}");
        if let Some(host) = var(&format!("{p}_FIX_HOST")) {
            out.push((
                format!("ibkr-{}", env_name.to_lowercase()),
                BrokerCredentials::IbkrFix {
                    host,
                    port: var(&format!("{p}_FIX_PORT")).and_then(|s| s.parse().ok()).unwrap_or(4001),
                    sender_comp_id: var(&format!("{p}_SENDER_COMP_ID")).unwrap_or_else(|| "OMS".into()),
                    target_comp_id: var(&format!("{p}_TARGET_COMP_ID")).unwrap_or_else(|| "IBKR".into()),
                    password: var(&format!("{p}_FIX_PASSWORD")).unwrap_or_default(),
                    ssl: var(&format!("{p}_FIX_SSL")).map(|s| s != "N").unwrap_or(true),
                },
            ));
        }

        let p = format!("BINANCE_{env_name}");
        if let (Some(host), Some(api_key), Some(path)) = (
            var(&format!("{p}_FIX_HOST")),
            var(&format!("{p}_API_KEY")),
            var(&format!("{p}_PRIVATE_KEY_PATH")),
        ) {
            // The store holds PEM bytes, not a path, so the file has to be read
            // now — while the operator is present to fix it if it is missing.
            match std::fs::read_to_string(&path) {
                Ok(private_key) => out.push((
                    format!("binance-{}", env_name.to_lowercase()),
                    BrokerCredentials::BinanceFix {
                        host,
                        port: var(&format!("{p}_FIX_PORT")).and_then(|s| s.parse().ok()).unwrap_or(9000),
                        sender_comp_id: var(&format!("{p}_SENDER_COMP_ID")).unwrap_or_else(|| "OMS".into()),
                        target_comp_id: var(&format!("{p}_TARGET_COMP_ID")).unwrap_or_else(|| "SPOT".into()),
                        api_key,
                        private_key,
                    },
                )),
                Err(e) => eprintln!("  skipped binance-{}: cannot read {path}: {e}", env_name.to_lowercase()),
            }
        }
    }

    out
}

/// Feed credentials found in the environment, keyed by `feed_connection.code`.
pub fn scan_feed_env() -> Vec<(String, FeedCredentials)> {
    match var("DATABENTO_API_KEY") {
        Some(api_key) => vec![("databento-opra".to_string(), FeedCredentials::Databento { api_key })],
        None => Vec::new(),
    }
}

/// Import everything found. Returns how many credentials were stored.
pub async fn run(pool: &PgPool, key: &MasterKey) -> Result<usize, Box<dyn std::error::Error>> {
    let brokers = scan_env();
    let feeds = scan_feed_env();
    if brokers.is_empty() && feeds.is_empty() {
        println!("nothing to import — no broker or feed credentials found in the environment");
        return Ok(0);
    }

    let mut n = 0;
    for (code, cred) in &brokers {
        // The connection row must exist first: credentials attach to a configured
        // routing target, they do not create one.
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM oms.broker_connection WHERE code = $1")
                .bind(code)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            println!("  skipped {code}: no broker_connection row (the app creates one at boot when credentials are present)");
            continue;
        }
        save_broker(pool, key, code, cred).await?;
        println!("  imported {code}");
        n += 1;
    }
    for (code, cred) in &feeds {
        save_feed(pool, key, code, cred).await?;
        println!("  imported {code}");
        n += 1;
    }

    println!(
        "\nimported {n} credential(s). You can now delete the matching lines from .env —\n\
         the environment is no longer consulted for broker or feed credentials."
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        for k in [
            "ALPACA_ENV", "ALPACA_PAPER_API_KEY", "ALPACA_PAPER_API_SECRET",
            "ALPACA_LIVE_API_KEY", "ALPACA_LIVE_API_SECRET",
            "BINANCE_PAPER_FIX_HOST", "BINANCE_PAPER_API_KEY", "BINANCE_PAPER_PRIVATE_KEY_PATH",
            "IBKR_PAPER_FIX_HOST", "IBKR_PAPER_FIX_PASSWORD",
            "DATABENTO_API_KEY",
        ] {
            std::env::remove_var(k);
        }
    }

    /// Nothing set imports nothing — running this on a clean machine must not
    /// invent empty credentials.
    #[test]
    fn an_empty_environment_yields_nothing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let brokers = scan_env();
        let feeds = scan_feed_env();
        clear();
        assert!(brokers.is_empty());
        assert!(feeds.is_empty());
    }

    #[test]
    fn finds_an_alpaca_pair_under_its_connection_code() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "AKTEST");
        std::env::set_var("ALPACA_PAPER_API_SECRET", "SECRET");
        let found = scan_env();
        clear();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "alpaca-paper", "must match broker_connection.code");
    }

    /// A half-configured broker is a mistake, not a credential — importing it
    /// would store something that cannot authenticate.
    #[test]
    fn a_half_configured_broker_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "AKTEST"); // no secret
        let found = scan_env();
        clear();
        assert!(found.is_empty());
    }

    #[test]
    fn finds_both_alpaca_environments() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("ALPACA_PAPER_API_KEY", "P");
        std::env::set_var("ALPACA_PAPER_API_SECRET", "PS");
        std::env::set_var("ALPACA_LIVE_API_KEY", "L");
        std::env::set_var("ALPACA_LIVE_API_SECRET", "LS");
        let found = scan_env();
        clear();
        let codes: Vec<_> = found.into_iter().map(|(c, _)| c).collect();
        assert!(codes.contains(&"alpaca-paper".to_string()));
        assert!(codes.contains(&"alpaca-live".to_string()));
    }

    #[test]
    fn finds_databento() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("DATABENTO_API_KEY", "db-xxx");
        let found = scan_feed_env();
        clear();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "databento-opra");
    }

    /// Binance's private key lives at a path in the environment and as PEM bytes
    /// in the store — an unreadable path must be skipped loudly, not stored empty.
    #[test]
    fn binance_with_an_unreadable_key_path_is_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("BINANCE_PAPER_FIX_HOST", "fix.example.com");
        std::env::set_var("BINANCE_PAPER_API_KEY", "K");
        std::env::set_var("BINANCE_PAPER_PRIVATE_KEY_PATH", "/nonexistent/nope.pem");
        let found = scan_env();
        clear();
        assert!(found.is_empty());
    }
}
