//! Guards on the line between the OMS and the gateway crate.

/// The gateway runs beside the core with no database and no Kafka, and so do
/// the crates a co-located strategy links in.
#[test]
fn gateway_depends_on_no_database_or_kafka() {
    for (krate, manifest) in [
        ("gateway", include_str!("../crates/gateway/Cargo.toml")),
        ("colo-risk", include_str!("../crates/colo-risk/Cargo.toml")),
        ("colo-link", include_str!("../crates/colo-link/Cargo.toml")),
    ] {
        for dep in ["sqlx", "rdkafka", "axum"] {
            assert!(!manifest.contains(dep), "crates/{krate} depends on {dep}");
        }
    }
}

/// A connector is one directory in the gateway; nothing of it is left in the OMS.
#[test]
fn a_connector_is_one_directory() {
    let root = env!("CARGO_MANIFEST_DIR");
    for c in gateway::connectors::all() {
        let code = c.manifest().code.to_lowercase();
        let dir = format!("{root}/crates/gateway/src/connectors/{code}");
        assert!(std::path::Path::new(&format!("{dir}/mod.rs")).exists(), "{dir}/mod.rs missing");
        for left in [
            format!("src/{code}_stream.rs"),
            format!("src/{code}_feed.rs"),
            format!("src/adapters/{code}.rs"),
            format!("src/fix/{code}.rs"),
        ] {
            assert!(!std::path::Path::new(&format!("{root}/{left}")).exists(), "{left} is still in the OMS");
        }
    }
}

/// The central files, in the OMS and the gateway's shared modules, work from
/// `all()` and never name a connector, so adding one touches its directory, its
/// line in `all()`, its migration and its docs page — nothing here. Comments
/// and test modules may name connectors (test fixtures need concrete ones);
/// module declarations are wiring.
#[test]
fn central_files_name_no_connector() {
    let central = [
        ("reload.rs", include_str!("reload.rs")),
        ("admin.rs", include_str!("admin.rs")),
        ("main.rs", include_str!("main.rs")),
        ("venues.rs", include_str!("venues.rs")),
        ("credentials.rs", include_str!("credentials.rs")),
        ("credentials_api.rs", include_str!("credentials_api.rs")),
        ("setup/brokers.rs", include_str!("setup/brokers.rs")),
        ("jobs/api.rs", include_str!("jobs/api.rs")),
        ("daily_stats_poller.rs", include_str!("daily_stats_poller.rs")),
        ("feeds.rs", include_str!("feeds.rs")),
        ("gateway/lib.rs", include_str!("../crates/gateway/src/lib.rs")),
        ("gateway/contract.rs", include_str!("../crates/gateway/src/contract.rs")),
        ("gateway/adapter.rs", include_str!("../crates/gateway/src/adapter.rs")),
        ("gateway/credentials.rs", include_str!("../crates/gateway/src/credentials.rs")),
        ("gateway/venue.rs", include_str!("../crates/gateway/src/venue.rs")),
        ("gateway/verify.rs", include_str!("../crates/gateway/src/verify.rs")),
        ("gateway/stream_health.rs", include_str!("../crates/gateway/src/stream_health.rs")),
        ("gateway/stream_supervisor.rs", include_str!("../crates/gateway/src/stream_supervisor.rs")),
        ("gateway/fix/mod.rs", include_str!("../crates/gateway/src/fix/mod.rs")),
        ("gateway/fix/app.rs", include_str!("../crates/gateway/src/fix/app.rs")),
        ("gateway/fix/adapter.rs", include_str!("../crates/gateway/src/fix/adapter.rs")),
        ("gateway/fix/dialect.rs", include_str!("../crates/gateway/src/fix/dialect.rs")),
    ];
    // case-insensitive, and `_` separates words: `alpaca_stream`,
    // `"BINANCE_ENV"` and `IbkrFix` all name a connector
    let names: Vec<String> = gateway::connectors::all()
        .iter()
        .flat_map(|c| {
            let m = c.manifest();
            [m.code.to_lowercase(), m.credential_kind.to_lowercase()]
        })
        .collect();
    // a word ends at a non-alphanumeric byte or where the next word starts
    // with a capital (`AlpacaAdapter`)
    let mentions = |line: &str, name: &str| {
        let orig = line.as_bytes();
        let lower = line.to_ascii_lowercase();
        lower.match_indices(name).any(|(i, _)| {
            let j = i + name.len();
            let starts = i == 0 || !orig[i - 1].is_ascii_alphanumeric() || orig[i].is_ascii_uppercase();
            let ends = j == orig.len() || !orig[j].is_ascii_alphanumeric() || orig[j].is_ascii_uppercase();
            starts && ends
        })
    };
    for (file, source) in central {
        // test modules may name connectors, wherever in the file they sit
        let mut in_test = false;
        let mut depth = 0i32;
        let mut after_cfg_test = false;
        for (n, line) in source.lines().enumerate() {
            let t = line.trim_start();
            if in_test {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                in_test = depth > 0;
                continue;
            }
            let item = t.trim_start_matches("pub(crate) ").trim_start_matches("pub ");
            if after_cfg_test && item.starts_with("mod ") && t.ends_with('{') {
                (in_test, depth) = (true, 1);
                continue;
            }
            after_cfg_test = t == "#[cfg(test)]";
            if t.starts_with("//") || t.starts_with("mod ") || t.starts_with("pub mod ") {
                continue;
            }
            for name in &names {
                assert!(!mentions(line, name), "{file}:{}: names connector {name}: {t}", n + 1);
            }
        }
    }
}

/// The order side carries no prices: previous closes come from `gateway::md`.
#[test]
fn order_adapter_carries_no_prices() {
    let adapter = include_str!("../crates/gateway/src/adapter.rs");
    assert!(!adapter.contains("daily_stats"), "BrokerAdapter still serves prices");
}

/// The pricing service has one source: no OMS file and no order-side gateway
/// file holds a quote sender; only `gateway::md` does. A connector's `feed.rs`
/// is its price side, which runs under `gateway::md` and is handed the sender.
#[test]
fn only_the_market_data_gateway_sends_quotes() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("crates/gateway/src/adapter.rs")];
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("crates/gateway/src/connectors"), &mut files);
    for f in files.iter().filter(|f| !f.ends_with("boundary_tests.rs") && !f.ends_with("feed.rs")) {
        let text = std::fs::read_to_string(f).unwrap();
        for word in ["Sender<Quote>", "Sender<dataprovider::Quote>", "quote_tx"] {
            assert!(!text.contains(word), "{} holds a quote sender ({word})", f.display());
        }
    }
}
