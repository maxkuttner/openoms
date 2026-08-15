//! Roles and database creation — the port of `db/scripts/provision.sh`.
//!
//! The script used `\gexec` to make DDL conditional. Here that is an existence
//! query followed by a statement, which is the same thing with a better error.
//!
//! `CREATE ROLE` and `CREATE DATABASE` cannot run inside a transaction, so each
//! statement is issued standalone against the `postgres` maintenance database.

use sqlx::{Connection, PgConnection};

use super::config::{PostgresConfig, RoleConfig};

/// Roles are fixed names, written literally into every migration (`SET ROLE
/// mdm_master`) and into `db/access/ods.sql`. Parameterising them would mean
/// templating 43 SQL files.
pub const ROLES: [&str; 2] = ["mdm_master", "oms_user"];

/// What is already present on the server. `init` refuses unless this is empty.
#[derive(Debug, Default)]
pub struct Existing {
    pub roles: Vec<String>,
    pub database: bool,
}

impl Existing {
    /// A fresh server: no roles of ours, no database of ours.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && !self.database
    }
}

/// Look for our roles and database without creating anything.
///
/// Connects to `postgres`, the maintenance database, because the target database
/// may not exist yet.
pub async fn inspect(cfg: &PostgresConfig) -> Result<Existing, sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    let mut roles = Vec::new();
    for role in ROLES {
        let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_roles WHERE rolname = $1")
            .bind(role)
            .fetch_optional(&mut conn)
            .await?;
        if found.is_some() {
            roles.push(role.to_string());
        }
    }

    let database: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&cfg.database)
        .fetch_optional(&mut conn)
        .await?;

    conn.close().await?;
    Ok(Existing { roles, database: database.is_some() })
}

/// Create both roles and the database, owned by `mdm_master`.
///
/// Assumes `inspect` already found nothing — the caller enforces strictness, so a
/// conflict here is a genuine race and surfaces as a Postgres error.
pub async fn provision(cfg: &PostgresConfig, roles: &RoleConfig) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    // Identifiers are the fixed constants above and passwords are quoted with
    // quote_literal by the server, so this cannot carry injectable input.
    for (role, password) in [
        ("mdm_master", &roles.mdm_password),
        ("oms_user", &roles.oms_password),
    ] {
        let stmt: String = sqlx::query_scalar("SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', $1, $2)")
            .bind(role)
            .bind(password)
            .fetch_one(&mut conn)
            .await?;
        sqlx::raw_sql(&stmt).execute(&mut conn).await?;
    }

    // mdm_master must exist before it can own the database.
    let stmt: String = sqlx::query_scalar("SELECT format('CREATE DATABASE %I OWNER mdm_master', $1)")
        .bind(&cfg.database)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;

    conn.close().await?;
    Ok(())
}

/// Drop the database. Roles are left alone — they are cluster-wide and may own
/// objects elsewhere, and `db-reset` never dropped them either.
pub async fn drop_database(cfg: &PostgresConfig) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;
    let stmt: String = sqlx::query_scalar("SELECT format('DROP DATABASE IF EXISTS %I WITH (FORCE)', $1)")
        .bind(&cfg.database)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_existing_is_empty() {
        assert!(Existing { roles: vec![], database: false }.is_empty());
    }

    /// Either a leftover role or a leftover database means this is not a fresh
    /// server — `init` must refuse in both cases, not just when the database is
    /// there. A half-provisioned server is exactly the state that produces
    /// confusing failures later.
    #[test]
    fn any_leftover_is_not_empty() {
        assert!(!Existing { roles: vec!["oms_user".into()], database: false }.is_empty());
        assert!(!Existing { roles: vec![], database: true }.is_empty());
    }
}
