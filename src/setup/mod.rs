//! Setup / seeding subcommands invoked via `oms setup …`.

pub mod bootstrap;
pub mod brokers;
pub mod catalog;
pub mod database;
pub mod import_env;
pub mod init;

/// Postgres connection string for the runtime pool (the `oms` role), built the
/// same way `serve()` builds it: host/port/database from `POSTGRES_*` (or their
/// defaults), password from `OMS_PASSWORD` (or the built-in default). Shared by
/// the setup subcommands, which each build their own pool (no server / AppState).
pub(crate) fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    let cfg = database::config::resolve(Default::default());
    let password = database::config::resolve_role_password(None);
    Ok(cfg.runtime_url(&password))
}
