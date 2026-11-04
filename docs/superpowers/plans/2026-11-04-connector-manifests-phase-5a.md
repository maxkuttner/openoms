# Connector Manifests (Phase 5a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every broker connector (Alpaca, Binance, IBKR, Bybit, Coinbase, OKX) is described by one `Connector` implementation with a manifest, and the central files drive themselves from the list of connectors. Adding a connector then touches its own module, its line in the list, its migration and its docs page, and nothing else.

**Architecture:**
- **Connector modules:** `src/connectors/` holds the `Connector` trait, the `Manifest` (venue copy, credential fields, environments, capabilities), one module per connector, and `connectors::all()`, which has one line per connector.
- **Generic credentials:** stored credentials become a generic `{kind, fields}` value that serializes to exactly today's JSON, and each connector parses its own typed struct from it.
- **Central files become loops over `connectors::all()`:** `reload.rs` (registration, stream restarts), `admin.rs` (stream stops), `main.rs` (boot spawns, public feeds), `venues.rs`, `credentials.rs` / `credentials_api.rs`, `setup/brokers.rs`, `jobs/api.rs`, `daily_stats_poller.rs` and `feeds.rs`.

**Tech Stack:** Rust, serde/serde_json, existing adapters and streams (unchanged inside).

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md` on branch `docs/target-architecture`: §4.2 (gateways, manifest), §7 phase 5. Success check: "Adding a connector touches its own module, its manifest line, its migration and its docs page only."

## Global Constraints

- **Branch:** `feat/connector-manifests`, created from `feat/order-engine`.
- **Phase 5 is split. This plan is 5a.** 5b moves `src/connectors`, the adapters, streams, feeds and `src/fix` into `crates/gateway`, as its own plan once 5a is merged. That step is file moves plus `pub` boundaries; 5a makes it possible by removing the central `match`es.
- **No data migration, no API change:**
  - Stored credential blobs (`{"kind":"Okx","api_key":…}`, `{"kind":"IbkrFix",…}` and the rest) must decode and re-encode to the same JSON value.
  - `GET /venues` (the cockpit's connection form) must return the same JSON as before, venue by venue.
  - CLI flags and env-var names (`setup/import_env.rs`, `BINANCE_{ENV}_TRANSPORT`) are unchanged.
- **Behaviour is unchanged.** Every existing test in `reload.rs`, `reload_tests.rs`, `venues.rs`, `credentials*.rs`, `setup/*`, `daily_stats_poller.rs` and `feeds.rs` keeps passing. Tests that build `BrokerCredentials::X { .. }` literals switch to a helper; their assertions stay the same.
- **Out of scope:**
  - Databento/OPRA, the one price-only vendor with its own feed credential, stays as it is: `FeedCredentials`, `restart_databento_feed`, `feeds.rs` keeps its entry. Price-only connectors join the manifest in phase 6, the market data gateway.
  - Order *routing* is unchanged.
- **Connector codes (exact):** `ALPACA`, `BINANCE`, `IBKR`, `BYBIT`, `COINBASE`, `OKX`.
- **Credential kind tags (exact, stored in the DB):** `Alpaca`, `BinanceFix` (also used by REST Binance), `IbkrFix`, `Bybit`, `Coinbase`, `Okx`.
- **Stream codes stay as today:** `exec_stream_code(code, env)` must reproduce `alpaca_exec_stream_code` etc. exactly. Check the current format in `reload.rs:459-477` and pin it in a test.
- **Don't commit the user's files.** Stage explicitly. DB tests: `#[ignore …]`, dev DB, no DB creation.

## Review Focus

1. **An existing stored credential of every kind** loads after the change, and saving the connection without retyping secrets keeps them. Covered by Task 1 `stored_blobs_of_every_kind_round_trip` and Task 4 `blank_secret_keeps_the_stored_value_for_every_connector`.
2. **An old IBKR blob without `ssl`** still means `ssl = true`. Covered by Task 1 `ibkr_without_ssl_defaults_to_true`.
3. **A Coinbase connection labelled PAPER** is still refused, because it would trade real money. Covered by Task 3 `coinbase_paper_connection_is_refused` (the existing test, kept).
4. **A credential change on a FIX connection** (IBKR, or Binance on FIX transport) still needs a restart and carries the running session forward. REST connectors still apply on reload. Covered by the existing `fix_connections_need_a_restart_but_rest_ones_do_not` and `restart_required_carries_the_existing_fix_adapter_forward`, both kept green.
5. **A future connector added by the recipe** must not need any central file. Covered by Task 5 `central_files_name_no_connector`, which derives the names to look for from `connectors::all()` itself.

---

### Task 1: Credentials become generic and connector-parsed

**Files:**
- Modify: `src/credentials.rs`: `BrokerCredentials` becomes a struct, and the redacted view becomes driven by field secrecy.
- Modify, adding one typed struct each next to its adapter:
  - `src/adapters/{alpaca,binance,bybit,coinbase,okx}.rs`;
  - `src/fix/ibkr.rs` (`IbkrFixCredentials`);
  - `src/fix/binance.rs` or `adapters/binance.rs` (`BinanceCredentials`, kind `BinanceFix`).
- Modify every `BrokerCredentials::X` construction or match:
  - `reload.rs`, `fix/mod.rs`, `credentials_api.rs`, `setup/brokers.rs`, `setup/import_env.rs` and `admin.rs`;
  - the tests in those files, plus `reload_tests.rs`.

**Interfaces:**
- Produces:
  ```rust
  /// One connection's credential: its kind tag and its fields, stored as
  /// `{"kind": <tag>, ...fields}` — byte-for-byte the old tagged-enum JSON.
  #[derive(Serialize, Deserialize, Clone)]
  pub struct BrokerCredentials {
      pub kind: String,
      #[serde(flatten)]
      pub fields: serde_json::Map<String, serde_json::Value>,
  }
  impl BrokerCredentials {
      pub fn new(kind: &str, fields: serde_json::Map<String, serde_json::Value>) -> Self;
      /// The connector's own typed view (`OkxCredentials`, `IbkrFixCredentials`, …).
      pub fn parse<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error>;
      pub fn from_typed<T: Serialize>(kind: &str, typed: &T) -> Self;
  }
  ```
  - The typed structs keep today's field names and serde defaults (`#[serde(default = "default_true")] ssl` on IBKR) and derive `Serialize, Deserialize`.
  - The redacted view (`credentials.rs:~110`) becomes `fn redacted(&self, secret: impl Fn(&str) -> bool) -> Vec<RedactedField>`. Task 2 passes the manifest's secrecy flags; until then, a temporary table in `credentials.rs` keeps today's per-kind visibility. Task 2 deletes it.

- [ ] **Step 1: Write the failing tests** in `credentials.rs`:
  - `stored_blobs_of_every_kind_round_trip`: for each of the six JSON shapes as stored today (copy the shapes from the existing tests at `credentials.rs:440-560`), `serde_json::from_str::<BrokerCredentials>` then `serde_json::to_value` equals `serde_json::from_str::<Value>` of the input.
  - `ibkr_without_ssl_defaults_to_true`: the JSON from `credentials.rs:553` → `parse::<IbkrFixCredentials>()` has `ssl == true`.
  - `typed_round_trip_keeps_the_kind_tag`: `from_typed("Okx", &OkxCredentials{..})` serializes with `"kind":"Okx"`.
  - `unknown_kind_still_decodes_but_no_connector_parses_it`: `{"kind":"Nasdaq"}` decodes. This changes the existing test at line 670, which asserted an error: the refusal moves to registration, where no connector claims the kind. Rule it in the ledger.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms credentials` fails to compile: `parse` not found.
- [ ] **Step 3: Implement**, then migrate every listed call site to `creds.parse::<Typed>()` / `BrokerCredentials::from_typed(..)`. Mechanical; behaviour identical.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green, and `grep -rn "BrokerCredentials::[A-Z]" src` returns nothing.
- [ ] **Step 5: Commit**: `refactor(credentials): generic stored credentials parsed by each connector`.

### Task 2: The `Connector` trait, manifests, and venues from them

**Files:**
- Create: `src/connectors/mod.rs` (trait, `Manifest`, `all()`, `by_code`, `by_credential_kind`).
- Create: `src/connectors/{alpaca,binance,ibkr,bybit,coinbase,okx}.rs`, each holding its manifest. Its venue copy, fields, environments and docs slug move here verbatim from `venues.rs:107-207`.
- Modify: `src/venues.rs`. `venues()` = `connectors::all().map(|c| c.manifest().venue())`, and `environment_allowed` reads the manifest. Its types (`Venue`, `CredentialField`, `Input`, `Environment`) stay where they are and become `pub` for the connector modules.
- Modify: `src/credentials.rs`. The redaction secrecy table from Task 1 is deleted; redaction asks `connectors::by_credential_kind(kind)`.
- Modify: `src/main.rs` (`mod connectors;`).

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Debug)]
  pub enum FieldType { Text, Port, Bool { default: bool } }

  pub struct Manifest {
      pub code: &'static str,              // "OKX"
      pub credential_kind: &'static str,   // "Okx"
      pub venue: fn() -> crate::venues::Venue,   // the exact Venue venues() returned before
      pub field_types: &'static [(&'static str, FieldType)], // non-Text fields only (IBKR/Binance: port, ssl)
      pub live_only: bool,                 // Coinbase
      pub universe: bool,                  // has an instrument universe to load (setup/brokers, jobs)
      pub quotes: QuoteScope,              // daily stats: which venues' instruments this broker prices
  }
  pub enum QuoteScope { OwnVenue, NonCrypto }   // crypto brokers: own venue; Alpaca: everything not crypto

  pub trait Connector: Send + Sync {
      fn manifest(&self) -> &'static Manifest;
  }
  pub fn all() -> &'static [&'static dyn Connector];   // one line per connector
  pub fn by_code(code: &str) -> Option<&'static dyn Connector>;
  pub fn by_credential_kind(kind: &str) -> Option<&'static dyn Connector>;
  ```
  Tasks 3 and 4 add trait methods; this task only moves data.

- [ ] **Step 1: Write the failing tests** in `connectors/mod.rs`:
  - `venues_json_is_unchanged`: `serde_json::to_value(crate::venues::venues())` equals a golden JSON captured **before** this task, by running the old `venues()` and saving the output to `src/connectors/testdata/venues.json`. Capture it as the very first action of the task, on the unchanged code.
  - `codes_and_kinds_are_unique`.
  - `every_connector_is_found_by_its_code_and_kind`.

  The existing `venues.rs` tests stay and must stay green.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** `cargo test --bin oms` is green.
- [ ] **Step 5: Commit**: `refactor(connectors): manifests own each venue's copy, fields and environments`.

### Task 3: Registration and execution streams through the connector

**Files:**
- Modify: `src/connectors/*.rs` (implement `connect`).
- Modify: `src/reload.rs`:
  - `build_registry`'s per-variant match becomes one call;
  - `RegistrationOutput` per-broker vectors become `exec_streams`;
  - the five `*_exec_stream_code` fns become `exec_stream_code`;
  - the four `restart_*_stream` fns become `restart_exec_stream`;
  - `ReloadStreams` gets one method `exec(..)` plus the existing `databento`;
  - `classify`'s FIX rule reads `Connected::session_bound` / the manifest.
- Modify: `src/admin.rs`. The per-broker stop block (`~1429-1470`) becomes a loop over connectors with an exec stream.
- Modify: `src/main.rs`. The boot spawns (Alpaca restart loop, Binance REST stream, Bybit/Coinbase/OKX restarts) become one loop over `exec_streams`.
- Modify: `src/adapters/mod.rs`. `register_alpaca`/`get_alpaca` and the Alpaca-only map are deleted: the generic `register`/`get` suffice once Alpaca's stream restarts from `ExecStream`.
- Modify: `src/reload_tests.rs` (`FakeStreams` implements the new trait shape).

**Interfaces:**
- Consumes: Tasks 1–2.
- Produces:
  ```rust
  /// What connecting one configured connection produced.
  pub struct Connected {
      pub adapter: Arc<dyn BrokerAdapter>,
      /// The execution stream to (re)start for it, if it has one apart from the adapter
      /// (REST connectors: Alpaca/Binance-REST/Bybit/Coinbase/OKX). FIX sessions start
      /// inside `connect` and return None.
      pub exec_stream: Option<ExecStream>,
  }
  /// Starts one execution stream; called again on every restart.
  pub type ExecStream = Arc<dyn Fn(StreamHandle, EngineHandle) -> BoxFuture<'static, ()> + Send + Sync>;

  // on Connector:
  /// Build the adapter (and start a FIX session) for one connection. Err = the
  /// `ConnectionOutcome::Failed` reason, worded as today.
  fn connect(&self, env: &'static str, creds: &BrokerCredentials, deps: &RegistrationDeps) -> Result<Connected, String>;
  /// True when a running session must be carried forward instead of rebuilt on a
  /// credential change (IBKR always; Binance when its transport is FIX).
  fn session_bound(&self, env: &'static str) -> bool;

  pub fn exec_stream_code(code: &str, env: &str) -> String;   // same strings as today
  pub struct ExecStreamStart { pub code: &'static str, pub env: &'static str, pub start: ExecStream }
  pub async fn restart_exec_stream(s: &ExecStreamStart, deps: &RegistrationDeps, streams: &StreamRegistry);
  ```
  - `build_registry` keeps every comment-documented rule: carry-forward on `Error`/`RestartRequired`, the kind-mismatch warning (expected kind from `by_credential_kind`), Coinbase LIVE-only (from `manifest.live_only`, reason string unchanged), and unknown kind → `Failed("no connector for credential kind {kind}")`.

- [ ] **Step 1: Write the failing tests:**
  - `exec_stream_codes_are_unchanged`: for each code with a stream and env PAPER/LIVE, `exec_stream_code` equals the old fn's output. Copy the old outputs into the test as literals before deleting the fns.
  - `unknown_credential_kind_is_reported_failed`: a `{"kind":"Nasdaq"}` connection → `Failed(..)`, nothing registered.
  - `every_rest_connector_hands_back_an_exec_stream`: a configured connection of each REST connector (reuse `reload.rs` test fixtures; Coinbase LIVE) yields one `ExecStreamStart` with its code.

  The existing `reload.rs` and `reload_tests.rs` tests stay green; adapt only their construction to the new types.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.** Move each connector's arm of `build_registry` into its `connect`, verbatim apart from types. Binance's transport switch (`Transport::from_env`) moves into `connectors/binance.rs`.
- [ ] **Step 4: Run.** `cargo test --bin oms` is green, and `cargo test --bin oms -- --ignored reload` is skipped (it needs a disposable DB; same ruling as phase 1).
- [ ] **Step 5: Commit**: `refactor(reload): register connections and restart their streams through connectors`.

### Task 4: Credential forms, universes, daily stats and public feeds through the connector

**Files:**
- Modify: `src/credentials_api.rs`:
  - `parse_broker` becomes generic over the manifest's fields and `field_types`, with the same merge rule (present replaces, blank keeps, missing with nothing stored → `MissingField`) and the same `BadField` wording for port/bool;
  - verification calls `Connector::verify`.
- Modify: `src/setup/brokers.rs`:
  - `enum Broker` and `ALL` go;
  - `build_*` move into each connector's `universe_provider`;
  - `known_broker_codes` and `brokers_with_creds` read the connectors;
  - the env-var-name helpers (`alpaca_env`…) move to each connector or stay, whichever keeps `import_env` unchanged.
- Modify: `src/jobs/api.rs` (code → connector lookup instead of the `match`).
- Modify: `src/daily_stats_poller.rs` (`CRYPTO_VENUES` and the code match become `manifest.quotes`).
- Modify: `src/feeds.rs` and `src/main.rs` (public feeds). Each connector with a public market-data feed declares it; `feeds::ALL` and the four boot spawn blocks are built from `connectors::all()` plus the Databento entry.

**Interfaces:**
- Produces, on `Connector` (defaults return `None`/`Err("not supported")`):
  ```rust
  /// Check a credential against the venue before it is saved (today's per-kind verify).
  fn verify<'a>(&'a self, env: &'static str, creds: &'a BrokerCredentials) -> BoxFuture<'a, Result<(), String>>;
  /// The data provider that loads this broker's instrument universe.
  fn universe_provider(&self, env: &'static str, creds: &BrokerCredentials) -> Result<Box<dyn dataprovider::DataProvider>, String>;
  /// The public quote feed, if this venue has one: its symbology and a session factory.
  fn public_feed(&self) -> Option<PublicFeed>;
  pub struct PublicFeed { pub symbology: &'static dyn dataprovider::FeedSymbology, pub session: fn(PgPool, mpsc::Sender<Quote>, broadcast::Receiver<OrderLaneEvent>, StreamHandle) -> Box<dyn Session> }
  ```
  - Match `provider_for`'s actual return type (`setup/brokers.rs:203`) and the verify signature in `credentials_api.rs` as they are today; the shapes above are the intent.

- [ ] **Step 1: Write the failing tests:**
  - `blank_secret_keeps_the_stored_value_for_every_connector`: for each connector, a stored credential plus a submission with every secret field blank → `parse_broker` returns the stored values.
  - `port_and_bool_fields_keep_their_validation`: IBKR with `port = "x"` gives `BadField{name: "port"}`, and `ssl = "maybe"` gives `BadField{name: "ssl"}`, with the same `why` text as today.
  - `quote_scope_matches_the_old_venue_rule`: for every (broker, venue) pair the old `daily_stats_poller.rs:40-44` match decided, the manifest-driven rule decides the same. Copy the old rule into the test as the oracle.
  - `public_feeds_are_the_four_crypto_venues`: the codes of `connectors::all()` with a `public_feed` are exactly BINANCE, BYBIT, COINBASE and OKX, and `feeds::ALL` still resolves every old code.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.**
  - `cargo test --bin oms` is green.
  - Then the boot check:
    1. Restart the dev OMS.
    2. The four `connecting stream="…/SPOT"` lines appear, plus ALPACA/PAPER, and the Binance FIX logon.
    3. `GET /venues` matches the golden file.
- [ ] **Step 5: Commit**: `refactor(connectors): credential forms, universes, daily stats and public feeds come from connectors`.

### Task 5: Guard the boundary; the recipe

**Files:**
- Modify: `src/connectors/mod.rs` tests.
- Create: `docs/contributing/adding-a-connector.md`.

- [ ] **Step 1: Write the test** `central_files_name_no_connector`.
  - For each central file, `include_str!` it: `reload.rs`, `reload_tests.rs`, `admin.rs`, `main.rs`, `venues.rs`, `credentials.rs`, `credentials_api.rs`, `setup/brokers.rs`, `jobs/api.rs`, `daily_stats_poller.rs`, `feeds.rs` and `adapters/mod.rs`.
  - Assert none contains, outside comments, any connector's `code`, `credential_kind`, or its module name in `snake_case` (derived from `connectors::all()`).
  - Allowed exception: `feeds.rs`'s Databento line, which is out of scope.
  - Strip `//` comment lines before matching.
  - Mutation check: add `"OKX"` to `daily_stats_poller.rs` code and confirm the test fails, then revert.
- [ ] **Step 2: Write the recipe.** It is a numbered list of the four touches:
  1. `src/connectors/<name>.rs` (manifest plus `Connector` impl, with links to an existing connector as the template);
  2. one line in `connectors::all()`;
  3. a migration (venue row, `provider_feed_policy` if it quotes);
  4. a docs page under the docs site's integrations section.

  Link it from `docs/superpowers/specs/2026-11-04-target-architecture-design.md`? No: that file lives on another branch. Link it from the repo README's developer section if one exists, otherwise from nowhere.
- [ ] **Step 3: Run.** `cargo test --bin oms` is green.
- [ ] **Step 4: Commit**: `test(connectors): central files may not name a connector; document the recipe`.
