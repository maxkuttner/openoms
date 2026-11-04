//! Guards on the line between the OMS and the gateway crate.

/// The gateway runs beside the core with no database and no Kafka of its own.
#[test]
fn gateway_depends_on_no_database_or_kafka() {
    let manifest = include_str!("../crates/gateway/Cargo.toml");
    for dep in ["sqlx", "rdkafka", "axum"] {
        assert!(!manifest.contains(dep), "crates/gateway depends on {dep}");
    }
}
