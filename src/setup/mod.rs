//! Setup / seeding subcommands invoked via `oms setup …`.

pub mod bootstrap;
pub mod brokers;
pub mod catalog;
pub mod database;

/// Postgres connection string for the runtime pool (`oms_user`), built the same
/// way `serve()` builds it: host/port/database from `POSTGRES_*` (or their
/// defaults), password from `OMS_USER_PASSWORD` (or the built-in default).
/// Shared by the setup subcommands, which each build their own pool (no server /
/// AppState).
pub(crate) fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    let cfg = database::config::resolve(Default::default());
    let roles = database::config::resolve_roles(None, None);
    Ok(cfg.runtime_url(&roles.oms_password))
}
