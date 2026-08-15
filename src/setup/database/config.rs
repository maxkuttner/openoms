//! Connection settings, merged from the three places they can come from.
//!
//! Precedence is CLI flag → environment variable → built-in default, so a fresh
//! clone works against a local Postgres with no configuration at all and a real
//! deployment overrides whatever it needs. This is why `.env` is an override file
//! here rather than a prerequisite.

use std::env;

/// Password given to both provisioned roles when nothing else is configured.
/// Localhost-only by construction — `preflight` refuses to serve with this value
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
/// runtime pool connects as `oms_user` instead.
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
}

/// Passwords for the two roles `init` creates.
#[derive(Debug, Clone)]
pub struct RoleConfig {
    pub mdm_password: String,
    pub oms_password: String,
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

pub fn resolve_roles(mdm: Option<String>, oms: Option<String>) -> RoleConfig {
    RoleConfig {
        mdm_password: mdm
            .or_else(|| from_env("MDM_MASTER_PASSWORD"))
            .unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into()),
        oms_password: oms
            .or_else(|| from_env("OMS_USER_PASSWORD"))
            .unwrap_or_else(|| DEFAULT_ROLE_PASSWORD.into()),
    }
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

    /// Is this server on the local machine? Gates the default-password check.
    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
    }
}

/// Environment keys this change removed, each with what replaced it.
///
/// Kept as an explicit list rather than deleted quietly: a `.env` carried over
/// from before the rename would otherwise be ignored silently, the defaults would
/// apply, and the failure would surface as an authentication error against the
/// wrong credentials.
const REMOVED_KEYS: [(&str, &str); 8] = [
    ("DATABASE_URL", "the URL is built from POSTGRES_* now"),
    ("ODS_DB", "use POSTGRES_DATABASE"),
    ("DB_HOST", "use POSTGRES_HOST"),
    ("DB_PORT", "use POSTGRES_PORT"),
    ("DB_NAME", "use POSTGRES_DATABASE"),
    ("DB_USER", "the runtime pool always connects as oms_user"),
    ("DB_PASSWORD", "use OMS_USER_PASSWORD"),
    ("ADMIN_USER", "use POSTGRES_USERNAME"),
];

/// One message per obsolete key that is still set. Empty means the environment is
/// clean.
pub fn check_removed_env_keys() -> Vec<String> {
    REMOVED_KEYS
        .iter()
        .filter(|(key, _)| from_env(key).is_some())
        .map(|(key, clause)| format!("{key} is no longer read — {clause}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise env mutation: these tests share process-global state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_env() {
        for k in [
            "POSTGRES_HOST", "POSTGRES_PORT", "POSTGRES_USERNAME",
            "POSTGRES_PASSWORD", "POSTGRES_DATABASE",
            "MDM_MASTER_PASSWORD", "OMS_USER_PASSWORD",
        ] {
            std::env::remove_var(k);
        }
    }

    /// Nothing configured at all must still produce a usable localhost config —
    /// that is what makes `.env` optional.
    #[test]
    fn falls_back_to_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
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
        let _g = ENV_LOCK.lock().unwrap();
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
        let _g = ENV_LOCK.lock().unwrap();
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
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("POSTGRES_PORT", "not-a-number");
        let c = resolve(PostgresOverrides::default());
        assert_eq!(c.port, 5432, "falls back rather than panicking");
        clear_env();
    }

    #[test]
    fn role_passwords_follow_the_same_tiers() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        assert_eq!(resolve_roles(None, None).oms_password, DEFAULT_ROLE_PASSWORD);
        std::env::set_var("OMS_USER_PASSWORD", "from-env");
        assert_eq!(resolve_roles(None, None).oms_password, "from-env");
        assert_eq!(
            resolve_roles(None, Some("from-flag".into())).oms_password,
            "from-flag"
        );
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

    fn sample() -> PostgresConfig {
        PostgresConfig {
            host: "localhost".into(),
            port: 5432,
            username: "postgres".into(),
            password: "postgres".into(),
            database: "ods".into(),
        }
    }

    /// A renamed key left in .env is silent otherwise: the value is ignored, the
    /// default is used, and the connection fails somewhere confusing. Naming the
    /// replacement turns that into one obvious line.
    #[test]
    fn reports_removed_keys_with_their_replacements() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::remove_var("DB_PASSWORD");
        assert!(check_removed_env_keys().is_empty());

        std::env::set_var("DB_PASSWORD", "x");
        std::env::set_var("DATABASE_URL", "y");
        let found = check_removed_env_keys();
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|m| m.contains("DB_PASSWORD") && m.contains("OMS_USER_PASSWORD")));
        assert!(found.iter().any(|m| m.contains("DATABASE_URL") && m.contains("POSTGRES_*")));

        std::env::remove_var("DB_PASSWORD");
        std::env::remove_var("DATABASE_URL");
    }
}
