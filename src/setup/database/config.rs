//! Connection settings, merged from the three places they can come from.
//!
//! Precedence is CLI flag → environment variable → built-in default, so a fresh
//! clone works against a local Postgres with no configuration at all and a real
//! deployment overrides whatever it needs. This is why `.env` is an override file
//! here rather than a prerequisite.

use std::env;

/// Password given to the `oms` role when nothing else is configured.
/// Localhost-only by construction — the server and `init` both refuse this value
/// against a non-loopback host.
pub const DEFAULT_ROLE_PASSWORD: &str = "openoms-dev";

/// CLI-supplied values. `None` means "not given", which defers to env then default.
#[derive(Debug, Default, Clone)]
pub struct PostgresOverrides {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
}

/// A resolved superuser connection. Used only by provisioning and teardown; the
/// runtime pool connects as the `oms` role instead.
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}



fn from_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

pub fn resolve(o: PostgresOverrides) -> PostgresConfig {
    PostgresConfig {
        host: o.host.or_else(|| from_env("POSTGRES_HOST")).unwrap_or_else(|| "localhost".into()),
        // A malformed port falls back rather than panicking: the connection will
        // fail with a clear address anyway, and panicking in a config getter gives
        // a worse message than the connection error does.
        port: o
            .port
            .or_else(|| from_env("POSTGRES_PORT").and_then(|p| p.parse().ok()))
            .unwrap_or(5432),
        username: o.username.or_else(|| from_env("POSTGRES_USERNAME")).unwrap_or_else(|| "postgres".into()),
        password: o.password.or_else(|| from_env("POSTGRES_PASSWORD")).unwrap_or_else(|| "postgres".into()),
        database: o.database.or_else(|| from_env("POSTGRES_DATABASE")).unwrap_or_else(|| "ods".into()),
    }
}

/// The password for the `oms` role, on the same flag → env → default tiers as
/// everything else.
pub fn resolve_role_password(flag: Option<String>) -> String {
    flag.or_else(|| from_env("OMS_PASSWORD")).unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into())
}

impl PostgresConfig {
    /// Connection URL for the configured database.
    pub fn url(&self) -> String {
        self.url_for(&self.database)
    }

    /// Connection URL for an arbitrary database on the same server. Provisioning
    /// needs this: it connects to `postgres` to create `ods`.
    pub fn url_for(&self, database: &str) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}?sslmode=disable",
            self.username, self.password, self.host, self.port, database
        )
    }

    /// Connection URL for the runtime pool: always the `oms` role, host/port/database
    /// taken from this config. This is the single place the runtime URL is built —
    /// `serve()` and `oms setup sync-broker` (via `setup::database_url`) both call
    /// it, so they cannot drift apart the way they did before. The password is
    /// percent-encoded so a role password containing `@`, `/`, `:` or `#` cannot
    /// corrupt the URL.
    pub fn runtime_url(&self, role_password: &str) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}?sslmode=disable",
            super::provision::ROLE,
            percent_encode_userinfo(role_password),
            self.host,
            self.port,
            self.database
        )
    }

    /// Is this server on the local machine? Gates the default-password check.
    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// Whether the shipped default password is being used somewhere it must not be:
    /// fine on a laptop, never against a host anything else can reach. Both `init`
    /// (which creates the role) and `serve` (which connects as it) ask this.
    pub fn refuses_default_password(&self, password: &str) -> bool {
        !self.is_loopback() && password == DEFAULT_ROLE_PASSWORD
    }
}

/// Loopback test for a bare hostname. Shared with the server's bind-address
/// check, which gates the default admin password the same way.
///
/// `0.0.0.0` is deliberately *not* loopback: binding it exposes the server to
/// every interface, which is exactly when a shipped default must be refused.
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Percent-encode a string for use as URL userinfo (RFC 3986). No dependency: the
/// safe set is small and fixed, so a byte-for-byte match against it is simpler
/// than pulling in a crate for it.
fn percent_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise env mutation: these tests share process-global state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_env() {
        for k in [
            "POSTGRES_HOST", "POSTGRES_PORT", "POSTGRES_USERNAME",
            "POSTGRES_PASSWORD", "POSTGRES_DATABASE", "OMS_PASSWORD",
        ] {
            std::env::remove_var(k);
        }
    }

    /// Nothing configured at all must still produce a usable localhost config —
    /// that is what makes `.env` optional.
    #[test]
    fn falls_back_to_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.host, "localhost");
        assert_eq!(c.port, 5432);
        assert_eq!(c.username, "postgres");
        assert_eq!(c.database, "ods");
    }

    /// Environment beats the built-in default.
    #[test]
    fn env_overrides_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        std::env::set_var("POSTGRES_HOST", "db.internal");
        std::env::set_var("POSTGRES_PORT", "6543");
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.host, "db.internal");
        assert_eq!(c.port, 6543);
        clear_env();
    }

    /// An explicit flag beats the environment — the whole point of the tier order.
    #[test]
    fn flag_overrides_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        std::env::set_var("POSTGRES_HOST", "from-env");
        let c = resolve(PostgresOverrides {
            host: Some("from-flag".to_string()),
            ..Default::default()
        });
        assert_eq!(c.host, "from-flag");
        clear_env();
    }

    /// An unparseable port falls back to 5432 — not a silent failure that connects
    /// somewhere unexpected.
    #[test]
    fn falls_back_on_unparseable_port() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        std::env::set_var("POSTGRES_PORT", "not-a-number");
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.port, 5432, "falls back rather than panicking");
        clear_env();
    }

    #[test]
    fn role_password_follows_the_same_tiers() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        assert_eq!(resolve_role_password(None), DEFAULT_ROLE_PASSWORD);
        std::env::set_var("OMS_PASSWORD", "from-env");
        assert_eq!(resolve_role_password(None), "from-env");
        assert_eq!(resolve_role_password(Some("from-flag".into())), "from-flag");
        clear_env();
    }

    /// Used by the preflight guard that refuses default passwords off-localhost.
    #[test]
    fn detects_loopback_hosts() {
        let c = |h: &str| PostgresConfig { host: h.into(), ..sample() };
        assert!(c("localhost").is_loopback());
        assert!(c("127.0.0.1").is_loopback());
        assert!(c("::1").is_loopback());
        assert!(!c("db.internal").is_loopback());
    }

    /// The connection URL must be usable for a database other than the configured
    /// one — provisioning connects to `postgres` before `ods` exists.
    #[test]
    fn builds_url_for_another_database() {
        assert!(sample().url_for("postgres").ends_with("/postgres?sslmode=disable"));
        assert!(sample().url().ends_with("/ods?sslmode=disable"));
    }

    /// The runtime URL is always the `oms` role, regardless of the configured
    /// superuser — this is what lets `serve()` and `database_url()` share it.
    #[test]
    fn runtime_url_always_connects_as_the_oms_role() {
        let url = sample().runtime_url("secret");
        assert!(url.starts_with("postgres://oms:secret@"));
        assert!(url.ends_with("/ods?sslmode=disable"));
    }

    /// A password containing URL-special characters must not corrupt the URL —
    /// each such character is percent-encoded rather than passed through raw.
    #[test]
    fn runtime_url_percent_encodes_special_characters() {
        let url = sample().runtime_url("p@ss/w:o#rd");
        assert!(url.contains("oms:p%40ss%2Fw%3Ao%23rd@"), "got {url}");
    }

    /// The shipped default is refused against anything but a loopback host, and
    /// accepted on one — that acceptance is what makes zero-config setup work.
    #[test]
    fn the_default_password_is_loopback_only() {
        let remote = PostgresConfig { host: "db.internal".into(), ..sample() };
        assert!(remote.refuses_default_password(DEFAULT_ROLE_PASSWORD));
        assert!(!remote.refuses_default_password("a-real-password"));
        assert!(!sample().refuses_default_password(DEFAULT_ROLE_PASSWORD));
    }

    fn sample() -> PostgresConfig {
        PostgresConfig {
            host: "localhost".into(),
            port: 5432,
            username: "postgres".into(),
            password: "postgres".into(),
            database: "ods".into(),
        }
    }

}
