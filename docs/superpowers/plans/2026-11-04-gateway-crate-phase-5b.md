# Gateway Crate (Phase 5b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move every connector's code (adapter, execution stream, public feed, FIX dialect) and the shared FIX engine out of the `rustoms` package into a new `crates/gateway` crate that has no database and no Kafka. Afterwards a new connector is one directory plus one line.

**Architecture:**
- **One crate, `gateway`.** It holds:
  - the contract types the core and the connectors share (`contract`);
  - the adapter trait and registry (`adapter`);
  - stream health and supervision;
  - the generic FIX engine (`fix`);
  - one directory per connector (`connectors/<code>/`), with its manifest, adapter, stream, feed and FIX dialect.
- **What stays in the OMS.** Everything that touches Postgres or Kafka: the order engine's `run` / `apply_exec`, recon's diff, `quote_feed.rs` (it reads held instruments from the database), credential encryption and storage, reload, admin and the HTTP API.
- **Feeds.** A connector hands back a quote source and the OMS drives it, so no `PgPool` crosses into the gateway.

**Tech Stack:** Rust 2021, Cargo workspace, tokio, quickfix (C++ / OpenSSL), the `dataprovider` crate.

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md`, on branch `docs/target-architecture`. This plan covers §4.2 and §7 phase 5, the crate half; phase 5a (manifests) is done on `feat/connector-manifests`.

## Global Constraints

- The gateways are crates in the same binary (one process). "Gateways (crates inside the binary; no DB, no Kafka)".
- `crates/gateway` may not depend on `sqlx`, `rdkafka` or `axum`, directly or through another workspace crate.
- Done when: "Adding a connector touches its own module, its manifest line, its migration and its docs page only".
- Single user, pre-release: no compatibility shims and no re-exports of old paths. OMS code says `gateway::…`.
- No help text in the UI. Docs per connector.
- DB tests stay `#[ignore]` against the shared dev DB. Never create databases on the shared dev Postgres. Never run the credential rotate DB test.
- Never touch Docker container `oms-postgres` or volume `oms-pgdata`.
- Never commit `.gitignore`, `.ignore` or `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`. Stage files explicitly; never use `git commit -a`. A heredoc and `git commit` never go in the same Bash call.
- Branch: `feat/gateway-crate`, cut from `feat/connector-manifests` (`2a13819`).
- Baseline: `cargo test` → 682 passed, 80 ignored. After every task, `cargo test --workspace` passes, with the same 80 ignored, and the passed count equals 682 plus the tests this plan adds.
- `OMS_OPENSSL_STATIC=1` release builds on macOS must keep carrying no Homebrew or OpenSSL load path (`.github/workflows/release.yml:92` checks this).
- These names must not change:
  - stream registry keys: `exec_stream_code`, feed labels `BINANCE/SPOT`, `BYBIT/SPOT`, `COINBASE/SPOT`, `OKX/SPOT`;
  - stored credential JSON;
  - `GET /venues` JSON (golden file `venues.json`);
  - the `fix::wire` log target.

## Review Focus

1. **macOS release link.** After quickfix moves into `gateway`, an `OMS_OPENSSL_STATIC=1` release build must still link OpenSSL statically. If the link-search path no longer reaches the final link, the binary either fails to link or quietly links `/opt/homebrew/.../libssl.3.dylib`, and the curl installer ships a binary that won't start. Test: Task 3, Step 7.
2. **`cargo test -p gateway` on its own.** The gateway's own test binary links quickfix too, so it needs the OpenSSL search path without help from the root `build.rs`. Test: Task 3, Step 6.
3. **Feed and stream names.** A changed supervisor label or exec-stream key makes the cockpit's health panel show a new entry and orphan the old one. Tests: `every_public_feed_keeps_its_label` (Task 2) and the existing `exec_stream_codes_are_unchanged`.
4. **Stored data still decodes.** Credential blobs written before the move must decode, and `/venues` must stay byte-identical. Tests: the existing `stored_blobs_of_every_kind_round_trip` and `venues_json_is_unchanged` move with their code and keep passing unchanged.
5. **A FIX session with a wrong credential.** `start_ibkr` / `start_binance` must still refuse another kind's credential once they live in their connector modules. Tests: the existing `start_ibkr_refuses_a_non_ibkr_credential`, `start_binance_refuses_a_non_binance_credential` and `start_binance_refuses_an_empty_fix_host` move with them in Task 2.

---

## File Structure (end state)

```
crates/gateway/
  Cargo.toml        no sqlx / rdkafka / axum
  build.rs          the OpenSSL link half of today's build.rs (quickfix is linked from here)
  src/lib.rs        pub mod contract, adapter, credentials, venue, verify, stream_health, stream_supervisor, fix, connectors;
  src/contract.rs   ExecEvent, ExecKind, ExecSource, VenueReport, GatewayMsg, EngineHandle,
                    ENGINE_CHANNEL_CAPACITY, BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState
  src/adapter.rs    today's src/adapters/mod.rs minus its `pub mod` lines
  src/credentials.rs BrokerCredentials (+impl), Shown          (from src/credentials.rs:18-64,111-118)
  src/venue.rs      Venue, Environment, Input, CredentialField, DOCS, FIX_SESSION, field(), environments()
                    (from src/venues.rs:14-96)
  src/verify.rs     TestOutcome, TEST_TIMEOUT, timed_out           (from src/credentials_api.rs)
  src/stream_health.rs, src/stream_supervisor.rs  moved as-is
  src/fix/{mod,app,adapter,dialect}.rs           generic engine only
  src/connectors/mod.rs                          Connector, Manifest, PublicFeed, QuoteSource, RegistrationDeps, all()
  src/connectors/<code>/mod.rs                   manifest + Connector impl  (today's src/connectors/<code>.rs)
  src/connectors/<code>/adapter.rs               today's src/adapters/<code>.rs   (alpaca, binance, bybit, coinbase, okx)
  src/connectors/<code>/stream.rs                today's src/<code>_stream.rs     (alpaca, binance, bybit, coinbase, okx)
  src/connectors/<code>/feed.rs                  today's src/<code>_feed.rs       (binance, bybit, coinbase, okx)
  src/connectors/<code>/fix.rs                   today's src/fix/<code>.rs        (binance, ibkr)
  src/connectors/testdata/venues.json            moved
src/boundary_tests.rs   (OMS) the two boundary guards; #[cfg(test)] mod in main.rs
```

What stays in the OMS and changes only its imports: `order_engine.rs`, which keeps `run`, `apply_exec`, `reports_for`, `needs_booked_state` and their tests. It also keeps `recon_orders.rs`, `quote_feed.rs`, `opra_stream.rs`, `venues.rs` (`venues()` and `environment_allowed`), credential storage and redaction, `credentials_api.rs`, `reload.rs`, `admin.rs`, `main.rs`, `feeds.rs`, `daily_stats_poller.rs`, `setup/*`, `jobs/*` and `handlers.rs`.

---

### Task 1: The gateway crate with the shared contract

**Files:**
- Create:
  - `crates/gateway/Cargo.toml`
  - `crates/gateway/src/lib.rs`
  - `crates/gateway/src/contract.rs`
  - `crates/gateway/src/adapter.rs`
  - `src/boundary_tests.rs`
- Move (`git mv`): `src/stream_health.rs` → `crates/gateway/src/stream_health.rs`; `src/stream_supervisor.rs` → `crates/gateway/src/stream_supervisor.rs`
- Modify:
  - root `Cargo.toml`: workspace member and `gateway = { path = "crates/gateway" }`
  - `src/adapters/mod.rs`: keeps only its five `pub mod` lines plus `pub use gateway::adapter::*;`, removed in Task 3
  - `src/order_engine.rs:28-162`: the contract types leave it
  - `src/recon_orders.rs:52-80`: the `Broker*` types leave it
  - every OMS file that imports the moved items (about 20, found by the compiler)

**Interfaces:**
- Produces:
  - `gateway::contract::{ExecEvent, ExecKind, ExecSource, GatewayMsg, EngineHandle, ENGINE_CHANNEL_CAPACITY, BrokerOpenOrder, BrokerOrderOutcome, BrokerOrderState, VenueReport}`, with today's fields and methods unchanged. `EngineHandle::{new, exec, reconcile}` and `ExecEvent::from_report(order_id: Uuid, venue: &str, actor: &'static str, report: VenueReport) -> ExecEvent`.
  - `gateway::adapter::{BrokerAdapter, BrokerRegistry, SplitAdapter, Transport, InstrumentProvider, BrokerInstrument, BrokerOrderRequest, BrokerOrderResponse, BrokerError}`.
  - `gateway::stream_health::*` and `gateway::stream_supervisor::*`.
- `VenueReport` is a gateway-owned copy of `domain::orders::commands::ExecutionReport`:
  - it has the same three variants and the same fields (`Fill { execution_id, fill_qty, fill_price, venue }`, `Reject { reason, venue }`, `Canceled { reason, venue }`);
  - it derives only `Debug, Clone, PartialEq`;
  - the gateway must not depend on the OMS's `domain` module.

  In this task, every stream and FIX file that builds an `ExecutionReport` to pass to `from_report` builds a `VenueReport` instead. The OMS's `ExecutionReport` is untouched.

- [ ] **Step 1: Write the failing test** in `src/boundary_tests.rs`, registered in `main.rs` as `#[cfg(test)] mod boundary_tests;`

```rust
/// The gateway runs beside the core with no database and no Kafka of its own.
#[test]
fn gateway_depends_on_no_database_or_kafka() {
    let manifest = include_str!("../crates/gateway/Cargo.toml");
    for dep in ["sqlx", "rdkafka", "axum"] {
        assert!(!manifest.contains(dep), "crates/gateway depends on {dep}");
    }
}
```

- [ ] **Step 2: Run it**

  Run: `cargo test gateway_depends_on_no_database_or_kafka`
  Expected: compile error "couldn't read `…/crates/gateway/Cargo.toml`".

- [ ] **Step 3: Create the crate and move the items**
  - **Dependencies:** only those the moved code uses (tokio, serde, serde_json, async-trait, arc-swap, uuid, tracing, futures-util, chrono). Add more in Task 3 as files arrive.
  - **Removing the old definitions:** delete them from `order_engine.rs` and `recon_orders.rs`.
  - **Rewriting OMS paths:** for example `crate::order_engine::{EngineHandle, ExecEvent}` becomes `gateway::contract::{EngineHandle, ExecEvent}`, and `crate::stream_health::` becomes `gateway::stream_health::`.
  - **Adapters:** `src/adapters/mod.rs` keeps `pub use gateway::adapter::*;` for this task only, so the five adapter files compile unchanged. Task 3 deletes the file.
  - **`missing_qty` assertions:** move the three in `coinbase_stream.rs:271-273` into a `recon_orders.rs` test named `missing_qty_books_only_what_is_new`, because `missing_qty` stays in the OMS.

- [ ] **Step 4: Run it**

  Run: `cargo test gateway_depends_on_no_database_or_kafka`
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: `cargo test --workspace 2>&1 | grep "test result"`
  Expected: every line `ok`. Passed across all lines = 682 + 2 (the boundary test and `missing_qty_books_only_what_is_new`), minus the moved test that `missing_qty_books_only_what_is_new` absorbed, if that test held only those asserts. Ignored = 80. Ledger the number; Tasks 2 and 3 count from it.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/gateway src/boundary_tests.rs src/main.rs src/order_engine.rs src/recon_orders.rs src/adapters src/stream_health.rs src/stream_supervisor.rs <every other file the compiler made you touch>
git commit -m "refactor(gateway): new gateway crate holds the execution contract, adapter trait and stream supervision"
```

---

### Task 2: Connector-specific code leaves the FIX engine and the feed driver

Still inside the OMS package. This makes the generic parts connector-free before they move.

**Files:**
- Modify:
  - `src/fix/mod.rs:118-120,300-410`: `start_ibkr` moves to `src/connectors/ibkr.rs` and `start_binance` to `src/connectors/binance.rs`, each with its three tests (Review Focus 5); `fix/mod.rs` keeps `start_session`
  - `src/fix/dialect.rs:219-300`: tests that build `IbkrDialect` / `BinanceDialect` move into `src/fix/ibkr.rs` / `src/fix/binance.rs`
  - `src/connectors/mod.rs:76-82`: new `PublicFeed`
  - `src/connectors/{binance,bybit,coinbase,okx}.rs`: `public_feed`
  - `src/quote_feed.rs:27-44`
  - `src/opra_stream.rs`
  - `src/main.rs`: feed loop
  - `src/connectors/mod.rs` tests: guard list
- Test: `src/connectors/mod.rs` tests

**Interfaces:**
- Produces, in `src/connectors/mod.rs` (moves verbatim in Task 3):

```rust
/// A live quote source the OMS drives: it knows what it can price and how to stream it.
pub trait QuoteSource: LiveQuoteFeed + FeedSymbology {}
impl<T: LiveQuoteFeed + FeedSymbology> QuoteSource for T {}

pub struct PublicFeed {
    pub symbology: &'static dyn FeedSymbology,
    /// Its supervisor's name in the stream registry, e.g. "OKX/SPOT".
    pub label: &'static str,
    pub source: fn() -> Arc<dyn QuoteSource>,
}
```

- `QuoteFeedSession::new(feed: Arc<dyn QuoteSource>, pool, out, order_rx, health)`: no longer generic.
- `main.rs` makes, per `connector.public_feed()`, `QuoteFeedSession::new((feed.source)(), …)`, then `stream_supervisor::supervise(feed.label, health, session)`, as the connector modules do today.
- `opra_stream.rs` wraps its feed in `Arc::new(…)`.
- `connectors::ibkr::start_ibkr(env, creds, health, engine)` and `connectors::binance::start_binance(...)` keep today's signatures.

- [ ] **Step 1: Write the failing tests** in `src/connectors/mod.rs` tests

```rust
#[test]
fn every_public_feed_keeps_its_label() {
    let labels: Vec<&str> = all().iter().filter_map(|c| c.public_feed()).map(|f| f.label).collect();
    assert_eq!(labels, ["BINANCE/SPOT", "BYBIT/SPOT", "COINBASE/SPOT", "OKX/SPOT"]);
}
```

  Also add `fix/mod.rs`, `fix/app.rs`, `fix/adapter.rs` and `fix/dialect.rs` to `central_files_name_no_connector`'s list.

- [ ] **Step 2: Run them**

  Run: `cargo test connectors::tests`
  Expected: `every_public_feed_keeps_its_label` fails to compile (no field `label`). Once that compiles, the guard fails with `fix/mod.rs` naming `ibkr`.

- [ ] **Step 3: Implement** the interfaces above. Move the two start functions, the dialect tests and the feed wiring.

- [ ] **Step 4: Run them**

  Run: `cargo test connectors::tests`
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: `cargo test --workspace 2>&1 | grep "test result"`
  Expected: all `ok`; Task 1's count + 1; 80 ignored.

- [ ] **Step 6: Commit**

```bash
git add src/fix src/connectors src/quote_feed.rs src/opra_stream.rs src/main.rs
git commit -m "refactor(connectors): FIX start-up and feed wiring move into each connector; the FIX engine and feed driver name none"
```

---

### Task 3: Every connector moves into the gateway crate

**Files:**
- Move (`git mv`, then fix paths):

  | from | to |
  |---|---|
  | `src/connectors/mod.rs` | `crates/gateway/src/connectors/mod.rs` |
  | `src/connectors/<code>.rs` | `crates/gateway/src/connectors/<code>/mod.rs` |
  | `src/connectors/testdata/` | `crates/gateway/src/connectors/testdata/` |
  | `src/adapters/<code>.rs` | `crates/gateway/src/connectors/<code>/adapter.rs` |
  | `src/adapters/testdata/` | under the connector that reads it |
  | `src/<code>_stream.rs` | `crates/gateway/src/connectors/<code>/stream.rs` |
  | `src/<code>_feed.rs` | `crates/gateway/src/connectors/<code>/feed.rs` |
  | `src/fix/{ibkr,binance}.rs` | `crates/gateway/src/connectors/{ibkr,binance}/fix.rs` |
  | `src/fix/{mod,app,adapter,dialect}.rs` | `crates/gateway/src/fix/` |

- Create:
  - `crates/gateway/src/credentials.rs`: `BrokerCredentials` + impl, `default_true`, `Shown`, and the tests `typed_round_trip_keeps_the_kind_tag`, `unknown_kind_still_decodes_but_no_connector_parses_it`, `ibkr_without_ssl_defaults_to_true`, `stored_blobs_of_every_kind_round_trip`;
  - `crates/gateway/src/venue.rs`: from `src/venues.rs:14-96`;
  - `crates/gateway/src/verify.rs`: `TestOutcome`, `TEST_TIMEOUT`, `timed_out`;
  - `crates/gateway/build.rs`: today's `build.rs` OpenSSL half.
- Modify:
  - `crates/gateway/Cargo.toml`: add `quickfix = { version = "0.2", features = ["build-with-ssl"] }` and the HTTP / WS / signing dependencies the moved files use; remove them from the root `Cargo.toml` when the OMS no longer uses them;
  - root `build.rs`: keep `ensure_cockpit_dist` only;
  - `RegistrationDeps` moves from `src/reload.rs:79` to `gateway::connectors`;
  - `src/main.rs`: remove `mod adapters; mod fix; mod connectors;` and the nine `*_stream` / `*_feed` lines;
  - `src/boundary_tests.rs`: receives `central_files_name_no_connector` from `connectors/mod.rs`;
  - `docs/contributing/adding-a-connector.md`.
- Delete: `src/adapters/mod.rs`, and the `connectors_do_not_touch_the_database` test in `src/order_engine.rs:340-360`. The crate's dependency list now enforces it, and the Task 1 test pins it.

**Interfaces:**
- Consumes: Task 1's `gateway::contract`, `gateway::adapter` and `gateway::stream_*`; Task 2's `PublicFeed`, `QuoteSource` and the start functions.
- Produces:
  - `gateway::connectors::{Connector, Manifest, FieldType, QuoteScope, Connected, ExecStream, PublicFeed, QuoteSource, RegistrationDeps, all, by_code, by_credential_kind, exec_stream_code}`;
  - `gateway::credentials::{BrokerCredentials, Shown}`;
  - `gateway::venue::*`;
  - `gateway::verify::{TestOutcome, TEST_TIMEOUT, timed_out}`;
  - `gateway::fix::start_session`.
- Visibility: connector submodules are `pub` only where the OMS still names them. Today that is `setup/import_env.rs`'s typed credentials and `jobs`' universe types; the compiler lists the rest.
- Placement: `src/credentials.rs` keeps storage, encryption and redaction (`redaction(kind)`, `Redact`, `RedactedField`, …), importing `gateway::credentials::{BrokerCredentials, Shown}`. The redaction tests stay with it.

- [ ] **Step 1: Write the failing test** in `src/boundary_tests.rs`

```rust
/// A connector is one directory in the gateway; nothing of it is left in the OMS.
#[test]
fn a_connector_is_one_directory() {
    let root = env!("CARGO_MANIFEST_DIR");
    for c in gateway::connectors::all() {
        let code = c.manifest().code.to_lowercase();
        let dir = format!("{root}/crates/gateway/src/connectors/{code}");
        assert!(std::path::Path::new(&format!("{dir}/mod.rs")).exists(), "{dir}/mod.rs missing");
        for left in [format!("src/{code}_stream.rs"), format!("src/{code}_feed.rs"), format!("src/adapters/{code}.rs"), format!("src/fix/{code}.rs")] {
            assert!(!std::path::Path::new(&format!("{root}/{left}")).exists(), "{left} is still in the OMS");
        }
    }
}
```

  Move `central_files_name_no_connector` into the same file and repoint it:
  - its OMS paths become `include_str!("reload.rs")` and so on;
  - add the gateway's shared files: `../crates/gateway/src/{lib,contract,adapter,credentials,venue,verify,stream_health,stream_supervisor}.rs` and `../crates/gateway/src/fix/{mod,app,adapter,dialect}.rs`;
  - drop `adapters/mod.rs`;
  - names come from `gateway::connectors::all()`.

- [ ] **Step 2: Run it**

  Run: `cargo test a_connector_is_one_directory`
  Expected: compile error (`gateway::connectors` not found).

- [ ] **Step 3: Do the moves** in the table above, then fix paths until `cargo build` is clean:
  - inside the gateway, `crate::adapters::okx::` → `crate::connectors::okx::adapter::` and `crate::okx_stream::` → `crate::connectors::okx::stream::`;
  - in the OMS, `crate::connectors::` → `gateway::connectors::`.

  Add `mod adapter; mod stream; mod feed; mod fix;` lines (only those a connector has) to each `connectors/<code>/mod.rs`, so `connectors/mod.rs` keeps one `mod <code>;` per connector next to its `all()` entry.

- [ ] **Step 4: Run it**

  Run: `cargo test a_connector_is_one_directory central_files_name_no_connector`
  Expected: PASS.

- [ ] **Step 5: Run the workspace**

  Run: `cargo test --workspace > .superpowers/sdd/2026-11-04-gateway-crate-phase-5b/t3.log 2>&1; grep "test result" .superpowers/sdd/2026-11-04-gateway-crate-phase-5b/t3.log`
  Expected: all `ok`; the passed sum = Task 2's count + 1 (`a_connector_is_one_directory`) − 1 (`connectors_do_not_touch_the_database`); 80 ignored.

- [ ] **Step 6: Test the gateway on its own** (Review Focus 2)

  Run: `cargo test -p gateway 2>&1 | grep "test result"`
  Expected: `ok`, with a non-zero passed count; links without a manual `LIBRARY_PATH`.

- [ ] **Step 7: Check the release link** (Review Focus 1, ~5 min)

  Run: `OMS_OPENSSL_STATIC=1 cargo build --release && ! otool -L target/release/oms | grep -qi 'homebrew\|openssl' && echo STATIC_OK`
  Expected: `STATIC_OK`.

- [ ] **Step 8: Check the boot**
  1. Stop the dev OMS background task.
  2. Run `cargo build && ./target/debug/oms > /tmp/oms-server.log 2>&1` in the background.
  3. After 20 s, run `grep -E "registered|feed|exec stream|supervis" /tmp/oms-server.log | head -20`.

  Expected: the same lines as the 5a boot check: the Alpaca and Binance FIX registrations, the exec streams, and the four `*/SPOT` feeds plus OPRA. No `error`.

- [ ] **Step 9: Update the recipe** `docs/contributing/adding-a-connector.md`:
  - **The places a connector touches:** (1) `crates/gateway/src/connectors/<name>/` (`mod.rs` + `adapter.rs` / `stream.rs` / `feed.rs` / `fix.rs` as it needs), with "copy the closest" pointing at the directories; (2) its `mod <name>;` and `all()` line in `crates/gateway/src/connectors/mod.rs`; (3) the migration; (4) the docs page.
  - **Removals:** the "Until those files move … phase 5b" paragraph, and the separate adapter/stream/feed file paragraph.
  - **The guard sentence:** names `boundary_tests::central_files_name_no_connector`.
  - **New line:** the gateway has no database or Kafka (`boundary_tests::gateway_depends_on_no_database_or_kafka`); what needs either belongs in the OMS.

- [ ] **Step 10: Commit**

```bash
git add -A crates/gateway src Cargo.toml Cargo.lock build.rs docs/contributing/adding-a-connector.md
git status --short   # confirm .gitignore, .ignore and the 2026-09-28 plan are NOT staged; unstage if they are
git commit -m "refactor(gateway): every connector lives in crates/gateway as one directory; the OMS keeps the database side"
```
