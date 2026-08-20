//! Role and database creation.
//!
//! One role, `oms`, owns everything: both schemas and every table in them, and it
//! is also what the running server connects as. This mirrors NautilusTrader, where
//! `database init` creates a single role and grants it the schema outright — the
//! setup a user has to understand is one credential, not a privilege matrix.
//!
//! `CREATE ROLE` and `CREATE DATABASE` cannot run inside a transaction, so each
//! statement is issued standalone against the `postgres` maintenance database.

use sqlx::{Connection, PgConnection};

use super::config::PostgresConfig;

/// The one role this project creates. A fixed name, written literally into
/// `db/access/ods.sql` and the seed files' `SET ROLE`.
pub const ROLE: &str = "oms";

/// What is already present on the server. `init` refuses unless this is empty.
#[derive(Debug, Default)]
pub struct Existing {
    pub roles: Vec<String>,
    pub database: bool,
}

impl Existing {
    /// A fresh server: no role of ours, no database of ours.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && !self.database
    }
}

/// Look for our role and database without creating anything.
///
/// Connects to `postgres`, the maintenance database, because the target database
/// may not exist yet.
pub async fn inspect(cfg: &PostgresConfig) -> Result<Existing, sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_roles WHERE rolname = $1")
        .bind(ROLE)
        .fetch_optional(&mut conn)
        .await?;
    let roles = found.map(|_| vec![ROLE.to_string()]).unwrap_or_default();

    let database: Option<i32> = sqlx::query_scalar("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&cfg.database)
        .fetch_optional(&mut conn)
        .await?;

    conn.close().await?;
    Ok(Existing { roles, database: database.is_some() })
}

/// Create the `oms` role and the database it owns.
///
/// Assumes `inspect` already found nothing — the caller enforces strictness, so a
/// conflict here is a genuine race and surfaces as a Postgres error.
pub async fn provision(cfg: &PostgresConfig, password: &str) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(&cfg.url_for("postgres")).await?;

    // The identifier is the fixed constant above and the password is quoted by the
    // server with quote_literal, so this cannot carry injectable input.
    let stmt: String = sqlx::query_scalar("SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', $1, $2)")
        .bind(ROLE)
        .bind(password)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;

    // The role must exist before it can own the database.
    let stmt: String = sqlx::query_scalar("SELECT format('CREATE DATABASE %I OWNER %I', $1, $2)")
        .bind(&cfg.database)
        .bind(ROLE)
        .fetch_one(&mut conn)
        .await?;
    sqlx::raw_sql(&stmt).execute(&mut conn).await?;

    conn.close().await?;
    Ok(())
}

/// Drop the database. The role is left alone — it is cluster-wide and may own
/// objects in other databases.
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
        assert!(Existing::default().is_empty());
        assert!(!Existing { roles: vec!["oms".into()], database: false }.is_empty());
        assert!(!Existing { roles: vec![], database: true }.is_empty());
    }
}
