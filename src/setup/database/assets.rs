//! SQL and reference data compiled into the binary.
//!
//! `include_dir!` rather than reading from disk, so a released `oms` can provision
//! a database from anywhere — no repo checkout, no `OMS_DB_SCRIPTS_DIR`. The files
//! stay ordinary files in the tree; only the loading changes.

use include_dir::{include_dir, Dir};

static MIGRATIONS_PUBLIC: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/db/migrations/ods/public");
static MIGRATIONS_OMS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/db/migrations/ods/oms");

/// One migration stream: a schema and the role that owns its objects.
pub struct MigrationTarget {
    pub schema: &'static str,
    pub owner: &'static str,
}

/// Apply order matters — `oms` tables reference `public` ones.
pub const TARGETS: [MigrationTarget; 2] = [
    MigrationTarget { schema: "public", owner: "mdm_master" },
    MigrationTarget { schema: "oms", owner: "oms_user" },
];

/// Every migration for a target as `(filename, sql)`, sorted by filename.
///
/// Filename *is* the version — the numeric prefix orders them and the tracking
/// table keys on it, so sorting here is what guarantees 0003 precedes 0021.
pub fn migrations(target: &MigrationTarget) -> Vec<(String, &'static str)> {
    let dir = match target.schema {
        "public" => &MIGRATIONS_PUBLIC,
        "oms" => &MIGRATIONS_OMS,
        other => panic!("no embedded migrations for schema {other}"),
    };
    let mut out: Vec<(String, &'static str)> = dir
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "sql"))
        .map(|f| {
            let name = f.path().file_name().unwrap().to_string_lossy().into_owned();
            let sql = f.contents_utf8().expect("migration is not valid UTF-8");
            (name, sql)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Access policy, in apply order: role attributes first, then grants.
pub fn access_sql() -> [(&'static str, &'static str); 2] {
    [
        ("roles.sql", include_str!("../../../db/access/roles.sql")),
        ("ods.sql", include_str!("../../../db/access/ods.sql")),
    ]
}

/// Reference data, in apply order. Venues are seeded separately from the MIC CSV
/// (see `seed.rs`) and must land before calendars, which join to them.
pub fn seed_sql() -> [(&'static str, &'static str); 3] {
    [
        ("seed_currencies.sql", include_str!("../../../db/scripts/seed_currencies.sql")),
        ("seed_crypto_venues.sql", include_str!("../../../db/scripts/seed_crypto_venues.sql")),
        ("seed_calendars.sql", include_str!("../../../db/scripts/seed_calendars.sql")),
    ]
}

/// ISO 10383 Market Identifier Code registry, the source for `venue`.
pub const MIC_CSV: &str = include_str!("../../../db/data/ISO10383_MIC.csv");

#[cfg(test)]
mod tests {
    use super::*;

    /// Both schemas must be present, each with its owning role — migrations run
    /// as the owner so objects end up owned correctly.
    #[test]
    fn declares_both_migration_targets() {
        assert_eq!(TARGETS[0].schema, "public");
        assert_eq!(TARGETS[0].owner, "mdm_master");
        assert_eq!(TARGETS[1].schema, "oms");
        assert_eq!(TARGETS[1].owner, "oms_user");
    }

    /// Filename order is apply order. 0021 must never run before 0003.
    #[test]
    fn migrations_are_sorted_by_filename() {
        for t in &TARGETS {
            let names: Vec<String> = migrations(t).into_iter().map(|(n, _)| n).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "{} migrations out of order", t.schema);
        }
    }

    /// Guards against an empty embed — include_dir failing silently would make
    /// `init` report success having created no schema at all.
    #[test]
    fn embeds_every_migration() {
        assert_eq!(migrations(&TARGETS[0]).len(), 23, "public migrations");
        assert_eq!(migrations(&TARGETS[1]).len(), 20, "oms migrations");
        for t in &TARGETS {
            for (name, sql) in migrations(t) {
                assert!(name.ends_with(".sql"), "{name} is not .sql");
                assert!(!sql.trim().is_empty(), "{name} is empty");
            }
        }
    }

    #[test]
    fn embeds_access_and_seed_sql() {
        assert_eq!(access_sql().len(), 2);
        assert_eq!(seed_sql().len(), 3);
        for (name, sql) in access_sql().iter().chain(seed_sql().iter()) {
            assert!(!sql.trim().is_empty(), "{name} is empty");
        }
    }

    /// roles.sql sets the per-role search_path and must run before the grants.
    #[test]
    fn access_files_are_in_apply_order() {
        assert_eq!(access_sql()[0].0, "roles.sql");
        assert_eq!(access_sql()[1].0, "ods.sql");
    }

    #[test]
    fn embeds_the_mic_registry() {
        assert!(MIC_CSV.starts_with("\"MIC\","), "unexpected CSV header");
        assert!(MIC_CSV.lines().count() > 2000, "MIC registry looks truncated");
    }
}
