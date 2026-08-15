//! `oms database` — provisioning, migration and teardown.
//!
//! One file per phase, each replacing the shell script it was ported from. The
//! binary carries its own SQL (see `assets`), so a released `oms` provisions a
//! database without a repo checkout — which is what removes `psql` and `python3`
//! from the install requirements.

pub mod config;
pub mod assets;
pub mod provision;
pub mod migrate;
