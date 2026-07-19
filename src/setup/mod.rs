//! Setup / seeding subcommands invoked via `oms setup …`.

pub mod brokers;
pub mod catalog;
pub mod feeds;

use std::env;

/// Postgres connection string from the standard `DB_*` env vars. Shared by the
/// setup subcommands, which each build their own pool (no server / AppState).
pub(crate) fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    let var = |k: &str| env::var(k).map_err(|_| format!("{k} must be set"));
    Ok(format!(
        "postgres://{}:{}@{}:{}/{}?sslmode=disable",
        var("DB_USER")?,
        var("DB_PASSWORD")?,
        var("DB_HOST")?,
        var("DB_PORT")?,
        var("DB_NAME")?,
    ))
}
