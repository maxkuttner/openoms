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
use crate::secrets::{self, MasterKey};

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Broker credentials found in the environment, keyed by `broker_connection.code`.
pub fn scan_env() -> Vec<(String, BrokerCredentials)> {
    let mut out = Vec::new();

    for env_name in ["PAPER", "LIVE"] {
        let key_var = format!("ALPACA_{env_name}_API_KEY");
        let secret_var = format!("ALPACA_{env_name}_API_SECRET");
        let code = format!("alpaca-{}", env_name.to_lowercase());
        match (var(&key_var), var(&secret_var)) {
            (Some(key), Some(secret)) => out.push((code, BrokerCredentials::Alpaca { key, secret })),
            (Some(_), None) => eprintln!("  skipped {code}: {key_var} is set but {secret_var} is not"),
            (None, Some(_)) => eprintln!("  skipped {code}: {secret_var} is set but {key_var} is not"),
            (None, None) => {}
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
        let apikey_var = format!("{p}_API_KEY");
        let path_var = format!("{p}_PRIVATE_KEY_PATH");
        let code = format!("binance-{}", env_name.to_lowercase());
        let host_var = format!("{p}_FIX_HOST");
        let host = var(&host_var);
        let api_key = var(&apikey_var);
        let path = var(&path_var);
        // The API key + PEM path are what every Binance transport needs — REST
        // (the default: `BINANCE_{ENV}_TRANSPORT=rest`, unset, or absent) never
        // reads a FIX host at all, so requiring one here would make a
        // by-the-book REST setup un-importable. `BinanceFix` is the only stored
        // shape (see its doc comment in credentials.rs), so an absent FIX host
        // defaults the same way `start_binance` used to when nothing configured
        // one: empty host, port 9000, SenderCompID "OMS", TargetCompID "SPOT" —
        // fine for REST, and FIX transport will simply need a real host set
        // before it can be used (noted below, not silently accepted as ready).
        match (&api_key, &path) {
            (Some(_), Some(path)) => {
                // The store holds PEM bytes, not a path, so the file has to be
                // read now — while the operator is present to fix it if it is
                // missing.
                match std::fs::read_to_string(path) {
                    Ok(private_key) => {
                        if host.is_none() {
                            println!(
                                "  note: {code} imported without a FIX host — REST transport \
                                 (the default) works as-is; set BINANCE_{env_name}_FIX_HOST \
                                 before importing again if you intend to use FIX"
                            );
                        }
                        out.push((
                            code,
                            BrokerCredentials::BinanceFix {
                                host: host.unwrap_or_default(),
                                port: var(&format!("{p}_FIX_PORT")).and_then(|s| s.parse().ok()).unwrap_or(9000),
                                sender_comp_id: var(&format!("{p}_SENDER_COMP_ID")).unwrap_or_else(|| "OMS".into()),
                                target_comp_id: var(&format!("{p}_TARGET_COMP_ID")).unwrap_or_else(|| "SPOT".into()),
                                api_key: api_key.unwrap(),
                                private_key,
                            },
                        ));
                    }
                    Err(e) => eprintln!("  skipped {code}: cannot read {path}: {e}"),
                }
            }
            // A FIX host with neither credential set is the same kind of typo as
            // every other partial configuration below — it must be reported, not
            // fall through this arm silently just because it happens to be the
            // one field REST doesn't need. Truly nothing set (the common case:
            // this broker/environment isn't configured at all) stays silent.
            (None, None) if host.is_some() => {
                eprintln!("  skipped {code}: {host_var} is set but {apikey_var} and {path_var} are not");
            }
            (None, None) => {}
            _ => {
                let mut missing = Vec::new();
                if api_key.is_none() {
                    missing.push(apikey_var.as_str());
                }
                if path.is_none() {
                    missing.push(path_var.as_str());
                }
                eprintln!("  skipped {code}: missing {}", missing.join(", "));
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

/// Refuse the whole import if `key` cannot open something already sealed in
/// the store. Checked once, before any row is written: an operator who
/// resolves the wrong key (say, `rotate-key` printed a new one that was never
/// saved into `oms.toml`, and a restart under the stale key happened to
/// succeed — see BLOCKING 2/4 in the credential-store review) would otherwise
/// have this command seal freshly-imported `.env` credentials under a
/// *different* key than what is already stored, splitting the store across
/// two keys that no single master key can then open — not even `rotate-key`,
/// which needs one key that opens everything to re-wrap it.
///
/// Looking at exactly one existing blob is enough: every row is sealed under
/// the same master key (rotation re-wraps them all together, see rotate.rs),
/// so one row answers "is this the right key" for the whole store. `None`
/// means nothing is stored yet — a first import, nothing to conflict with.
async fn refuse_if_key_does_not_match_existing_store(
    pool: &PgPool,
    key: &MasterKey,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT code, credentials FROM oms.broker_connection WHERE credentials IS NOT NULL LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let existing = match existing {
        Some(row) => Some(row),
        None => {
            sqlx::query_as(
                "SELECT code, credentials FROM oms.feed_connection WHERE credentials IS NOT NULL LIMIT 1",
            )
            .fetch_optional(pool)
            .await?
        }
    };
    let Some((code, blob)) = existing else {
        return Ok(());
    };
    if secrets::open(key, &code, &blob).is_err() {
        return Err(format!(
            "refusing to import: the configured master key does not decrypt the existing \
             credential store (checked row '{code}'). Importing now would seal these new \
             credentials under a different key than what is already there, splitting the store \
             across two keys that no single master key can then read — not even `rotate-key`. \
             Fix oms.master_key (or OMS_MASTER_KEY) to match what the store was actually sealed \
             with before importing."
        )
        .into());
    }
    Ok(())
}

/// Import everything found. Returns how many credentials were stored.
pub async fn run(pool: &PgPool, key: &MasterKey) -> Result<usize, Box<dyn std::error::Error>> {
    let brokers = scan_env();
    let feeds = scan_feed_env();
    if brokers.is_empty() && feeds.is_empty() {
        println!("nothing to import — no broker or feed credentials found in the environment");
        return Ok(0);
    }

    // Before writing anything: a wrong-but-configured key must not be allowed
    // to split the store across two keys. See the function doc for the chain
    // that makes this reachable in practice.
    refuse_if_key_does_not_match_existing_store(pool, key).await?;

    // Seed `broker_connection` for the currently active `{BROKER}_ENV` routing
    // target before scanning existence below. On a fresh install this is the
    // *only* thing that creates these rows before the app has ever been
    // started — and `oms config import-env` is exactly the command the README
    // tells a new user to run right after `oms init`, before ever booting the
    // server. Idempotent, and the identical call `serve()` makes at boot (see
    // its doc comment in bootstrap.rs), so this cannot create anything a
    // normal boot wouldn't have.
    crate::setup::bootstrap::ensure_broker_connections(pool).await;

    let mut n = 0;
    for (code, cred) in &brokers {
        // The connection row must exist first: credentials attach to a configured
        // routing target, they do not create one. Past the `ensure_broker_connections`
        // call above, a missing row can only be IBKR (no `Broker` variant, so
        // never auto-created) or the *other* `{BROKER}_ENV` — e.g. `alpaca-paper`
        // when `ALPACA_ENV=LIVE` — which legitimately has no row until that
        // environment is the active one.
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM oms.broker_connection WHERE code = $1")
                .bind(code)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            println!("  skipped {code}: no broker_connection row (create one first via POST /admin/broker-connections, or switch *_ENV to it and re-run this import)");
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
    use crate::setup::brokers::Broker;

    /// Serialises env mutation across this module's tests. `setup::brokers::tests`
    /// has its own, separate `ENV_LOCK` — the two don't coordinate, and don't
    /// need to today, because every test on both sides only ever *clears*
    /// `ALPACA_ENV`/`BINANCE_ENV` (via `clear()` here, similarly there), never
    /// sets them. The moment a test in either module starts *setting* one of
    /// those vars rather than clearing it, that stops being safe — the two
    /// locks must be merged into one shared lock first, or tests in the two
    /// modules could interleave and see each other's env mutations.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Every variable `scan_env`/`scan_feed_env` read, both PAPER and LIVE, plus
    /// the `*_ENV` variables `Broker::connection_code()` reads — otherwise a
    /// developer machine exporting a LIVE or `*_ENV` variable could fail a test
    /// for reasons unrelated to the code under test.
    fn clear() {
        for k in [
            "ALPACA_ENV", "ALPACA_PAPER_API_KEY", "ALPACA_PAPER_API_SECRET",
            "ALPACA_LIVE_API_KEY", "ALPACA_LIVE_API_SECRET",
            "BINANCE_ENV",
            "BINANCE_PAPER_FIX_HOST", "BINANCE_PAPER_API_KEY", "BINANCE_PAPER_PRIVATE_KEY_PATH",
            "BINANCE_PAPER_FIX_PORT", "BINANCE_PAPER_SENDER_COMP_ID", "BINANCE_PAPER_TARGET_COMP_ID",
            "BINANCE_LIVE_FIX_HOST", "BINANCE_LIVE_API_KEY", "BINANCE_LIVE_PRIVATE_KEY_PATH",
            "BINANCE_LIVE_FIX_PORT", "BINANCE_LIVE_SENDER_COMP_ID", "BINANCE_LIVE_TARGET_COMP_ID",
            "IBKR_PAPER_FIX_HOST", "IBKR_PAPER_FIX_PORT", "IBKR_PAPER_SENDER_COMP_ID",
            "IBKR_PAPER_TARGET_COMP_ID", "IBKR_PAPER_FIX_PASSWORD", "IBKR_PAPER_FIX_SSL",
            "IBKR_LIVE_FIX_HOST", "IBKR_LIVE_FIX_PORT", "IBKR_LIVE_SENDER_COMP_ID",
            "IBKR_LIVE_TARGET_COMP_ID", "IBKR_LIVE_FIX_PASSWORD", "IBKR_LIVE_FIX_SSL",
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

    /// The codes this module hand-builds for Alpaca and Binance must agree with
    /// `Broker::connection_code()` byte-for-byte: a mismatch means every import
    /// silently finds no `broker_connection` row. IBKR has no `Broker` variant,
    /// so its code stays hand-built and isn't checked here.
    #[test]
    fn alpaca_and_binance_codes_match_broker_connection_code() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // ALPACA_ENV / BINANCE_ENV are cleared, so both default to "PAPER" —
        // matching the "PAPER" iteration of scan_env()'s loop.
        let alpaca_code = Broker::Alpaca.connection_code();
        let binance_code = Broker::Binance.connection_code();
        clear();
        assert_eq!(alpaca_code, "alpaca-paper");
        assert_eq!(binance_code, "binance-paper");
    }

    /// `BINANCE_{ENV}_FIX_HOST` set alone, with neither the API key nor the PEM
    /// path, used to fall into an empty `(None, None)` match arm and vanish
    /// with no message at all — every other partial Binance/Alpaca
    /// configuration reports a skip, this one alone didn't. The returned list
    /// was already empty either way (nothing here is enough to build a
    /// credential); what this pins is that the case is still recognised as a
    /// mistake worth naming — run with `-- --nocapture` to see the message.
    #[test]
    fn binance_fix_host_alone_is_still_skipped() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("BINANCE_PAPER_FIX_HOST", "fix.example.com");
        let found = scan_env();
        clear();
        assert!(found.is_empty(), "a FIX host alone cannot build a credential");
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

    /// THE regression this round exists to fix: `.env.example` documents Binance
    /// as `BINANCE_{ENV}_API_KEY` + `_PRIVATE_KEY_PATH` only — no FIX host,
    /// because REST is the default transport. A by-the-book REST setup must
    /// still import: the FIX-specific fields default (empty host, port 9000,
    /// "OMS"/"SPOT" comp ids) exactly the way `start_binance` used to when
    /// nothing configured them, rather than being refused for a field REST
    /// never reads.
    #[test]
    fn binance_without_a_fix_host_still_imports_for_rest_transport() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let pem_path = std::env::temp_dir().join("oms-test-binance-rest.pem");
        std::fs::write(&pem_path, "-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----")
            .expect("write pem");
        std::env::set_var("BINANCE_PAPER_API_KEY", "K");
        std::env::set_var("BINANCE_PAPER_PRIVATE_KEY_PATH", pem_path.to_str().unwrap());
        let found = scan_env();
        clear();
        let _ = std::fs::remove_file(&pem_path);

        assert_eq!(found.len(), 1);
        let (code, cred) = &found[0];
        assert_eq!(code, "binance-paper");
        match cred {
            BrokerCredentials::BinanceFix { host, port, sender_comp_id, target_comp_id, api_key, private_key } => {
                assert_eq!(host, "", "no FIX host configured — must default empty, not be refused");
                assert_eq!(*port, 9000);
                assert_eq!(sender_comp_id, "OMS");
                assert_eq!(target_comp_id, "SPOT");
                assert_eq!(api_key, "K");
                assert!(private_key.contains("test"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
