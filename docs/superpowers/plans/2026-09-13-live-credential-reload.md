# Live Credential Reload Implementation Plan

**Status (2026-09-13):** All six tasks implemented and individually reviewed;
final review fixes and HTTP/store verification complete. See
`../reports/2026-09-13-live-credential-reload-verification.md` for executed checks
and remaining boundaries. The task checkboxes below are the original execution
recipe; they are not the current status ledger.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply a changed credential without restarting the server — for the connections where that is actually safe — and give the cockpit a reload trigger to call.

**Architecture:** `AppState`'s `Arc<BrokerRegistry>` becomes `ArcSwap<BrokerRegistry>`, so a writer can publish a new registry while readers on the order path take an atomic load. The boot registration loop is extracted from `serve()` into a `reload` module that both boot and reload call, so a credential registered at startup and one registered at runtime cannot diverge. A new `POST /admin/connections/reload` re-reads the store and reports, per connection, what it did.

**Plan 3 of 4** for this spec. Plan 1 (`oms.toml` bootstrap) and Plan 2 (encrypted credential store) are merged. Plan 4 adds the credential write endpoints and cockpit screens that will call this reload.

**Tech Stack:** Rust 2021, `arc-swap` 1 (new), axum, tokio, sqlx 0.8.

**Spec:** `docs/superpowers/specs/2026-08-20-connection-config-gui-design.md`

## Global Constraints

- **Rust edition 2021.** Doc comments explain *why*, not *what*.
- **Secrets never reach a log, an error message, a `Debug`, or an HTTP response.** The reload endpoint reports connection codes and outcomes, never credential material.
- **Reload must never take the order path down.** A reader holding an adapter must keep working; a failed reload must leave the previous registry in place, not an empty one.
- **FIX sessions are NOT reloadable in this plan.** `fix::start_session` spawns an OS thread that ends in `loop { std::thread::park(); }` with no handle and no stop path, so a running IBKR or Binance-FIX session cannot be stopped. Attempting a second session to the same venue would collide on logon and sequence numbers. A FIX credential change must be reported as **needing a restart**, never silently ignored and never hot-swapped.
- **One code path builds an adapter.** Boot and reload call the same function.
- **`oms database init|migrate|drop|status` and `oms config import-env|rotate-key` keep their current behaviour.**
- **Tests needing Postgres are `#[ignore]`d**, matching `credentials.rs`'s round-trip test.
- **Never call `config::load()` or write an `oms.toml` from a test** — the repo root has a real one.

---

## What is and is not reloadable

This table is the plan's central claim. Task 2 encodes it in code and Task 5 reports it.

| Connection | Mechanism | Reloadable? |
|---|---|---|
| Alpaca PAPER/LIVE | REST adapter — an HTTP client behind an `Arc` | **Yes.** Swap the registry entry. |
| Databento OPRA feed | supervised task; the supervisor already treats a dropped stream as a reconnect | **Yes.** Abort the `JoinHandle`, respawn. |
| IBKR FIX | quickfix `Initiator` on a parked OS thread, no stop path | **No.** Report "restart required". |
| Binance FIX | same | **No.** Report "restart required". |
| Binance REST | REST adapter, same shape as Alpaca | **Yes.** Swap the registry entry. |

The Alpaca *execution stream* (`alpaca_stream::run`) is a spawned task holding its own key/secret copy. It is covered in Task 4 — swapping the adapter without restarting the stream would leave fills arriving on an old credential.

---

## File Structure

**Create:**
- `src/reload.rs` — the extracted registration logic and the reload entry point. One responsibility: turn the current contents of the credential store into a live registry and a live set of feed tasks, and say what changed.

**Modify:**
- `Cargo.toml` — add `arc-swap`.
- `src/app_state.rs` — `registry: Arc<ArcSwap<BrokerRegistry>>`, plus the stream-handle registry.
- `src/main.rs` — `serve()` delegates its registration loop to `src/reload.rs`; register the new route.
- `src/admin.rs` — the reload handler.

**Why a new module rather than more of `main.rs`:** `serve()` is already ~400 lines, and the registration loop is the part Plan 4 needs to call. Extracting it is what makes the reload endpoint a thin wrapper instead of a second copy of the logic.

---

### Task 1: The swappable registry

**Files:**
- Modify: `Cargo.toml`, `src/app_state.rs`, `src/main.rs`, `src/handlers.rs`

**Interfaces:**
- Consumes: `BrokerRegistry` (existing, `src/adapters/mod.rs`).
- Produces:
  - `AppState::registry(&self) -> arc_swap::Guard<Arc<BrokerRegistry>>`
  - `AppState::swap_registry(&self, next: BrokerRegistry)`

- [ ] **Step 1: Add the dependency**

```toml
arc-swap = "1"
```

- [ ] **Step 2: Write the failing test**

Add to `src/app_state.rs` (create a `#[cfg(test)] mod tests` if there is none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that already resolved an adapter must keep working across a swap.
    /// This is the property that makes reload safe on the order path: an order in
    /// flight holds its own `Arc` and finishes against the adapter it started
    /// with, rather than having it replaced underneath.
    #[test]
    fn a_held_adapter_survives_a_swap() {
        let state = sample_state();

        let before = state.registry();
        let had_alpaca = before.get_alpaca("PAPER").is_some();

        state.swap_registry(BrokerRegistry::new()); // empty: everything removed

        // The guard taken before the swap still sees the old registry.
        assert_eq!(before.get_alpaca("PAPER").is_some(), had_alpaca);
        // A guard taken after sees the new one.
        assert!(state.registry().get_alpaca("PAPER").is_none());
    }

    /// Two swaps in a row must both land — a writer must not be able to publish a
    /// stale registry over a newer one.
    #[test]
    fn the_last_swap_wins() {
        let state = sample_state();
        state.swap_registry(BrokerRegistry::new());
        let mut second = BrokerRegistry::new();
        second.register_alpaca("LIVE", sample_alpaca_adapter());
        state.swap_registry(second);
        assert!(state.registry().get_alpaca("LIVE").is_some());
    }
}
```

Write `sample_state()` and `sample_alpaca_adapter()` as small local helpers. `AlpacaAdapter::new(key, secret, environment)` needs no network to construct — check `src/adapters/alpaca.rs` and confirm before relying on it. If `AppState` is awkward to build in a test because it needs a `PgPool`, use `PgPool::connect_lazy` with a dummy URL: it builds a pool without connecting, and the test never issues a query. `src/fix/mod.rs`'s own tests already use that trick.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test app_state::tests`
Expected: FAIL to compile — `swap_registry` does not exist.

- [ ] **Step 4: Write the implementation**

In `src/app_state.rs`, change the field and the accessor:

```rust
use arc_swap::ArcSwap;

pub struct AppState {
    // …
    /// Swappable so a credential change can publish a new registry without
    /// restarting: readers on the order path take an atomic load, and an order
    /// already routing holds its own `Arc` and finishes against the adapter it
    /// started with. A `Mutex` here would put a lock on every order.
    registry: Arc<ArcSwap<BrokerRegistry>>,
    // …
}
```

In the constructor, `registry: Arc::new(ArcSwap::from_pointee(registry))`.

```rust
    /// The live registry. The returned guard derefs to `BrokerRegistry`, so
    /// existing call sites are unchanged; hold it only as long as needed.
    pub fn registry(&self) -> arc_swap::Guard<Arc<BrokerRegistry>> {
        self.registry.load()
    }

    /// Publish a new registry. Readers see it on their next `registry()` call;
    /// anything already holding an adapter is unaffected.
    pub fn swap_registry(&self, next: BrokerRegistry) {
        self.registry.store(Arc::new(next));
    }
```

- [ ] **Step 5: Fix the three call sites**

`grep -rn "\.registry()" src/` finds exactly three: `src/handlers.rs` (twice) and `src/main.rs` (once). Each calls a method that returns an owned `Arc<...>`, so the guard can be dropped immediately and the call sites should compile unchanged. If one does not, bind the guard to a local first rather than changing what it returns.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test` — expect the existing suite plus 2, all passing.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/app_state.rs src/handlers.rs src/main.rs
git commit -m "feat(state): make the broker registry swappable

ArcSwap rather than a Mutex: readers are on the order path and a lock there
would be paid on every order, while swaps happen only when a credential
changes. An order already routing holds its own Arc and finishes against
the adapter it started with, so a reload cannot yank a client out from
under an in-flight order."
```

---

### Task 2: Extract registration into a reloadable function

**Files:**
- Create: `src/reload.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `credentials::{load_brokers, load_feeds, Connection, CredentialState, BrokerCredentials}`, `config::master_key`, `fix::{start_ibkr, start_binance}`, `adapters::BrokerRegistry`.
- Produces:
  - `pub enum ConnectionOutcome { Registered, Unconfigured, Disabled, Failed(String), RestartRequired }`
  - `pub struct ReloadReport { pub connections: Vec<(String, ConnectionOutcome)> }` — deriving `Debug, serde::Serialize`, since Task 5 returns it as JSON
  - `pub fn classify(conn: &Connection<BrokerCredentials>, is_boot: bool) -> ConnectionOutcome`

This task is the heart of the plan. It moves the loop that currently lives inline in `serve()` (around `src/main.rs:814`) into a function that can run twice.

- [ ] **Step 1: Write the failing tests**

Create `src/reload.rs` with only tests. `classify` is pure, so all of this runs without a database:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{BrokerCredentials, Connection, CredentialState};

    fn conn(code: &str, status: &str, state: CredentialState<BrokerCredentials>) -> Connection<BrokerCredentials> {
        Connection {
            code: code.into(),
            kind: "ALPACA".into(),
            environment: Some("PAPER".into()),
            status: status.into(),
            credentials: state,
            credentials_updated_at: None,
        }
    }

    fn alpaca() -> BrokerCredentials {
        BrokerCredentials::Alpaca { key: "k".into(), secret: "s".into() }
    }

    fn ibkr() -> BrokerCredentials {
        BrokerCredentials::IbkrFix {
            host: "h".into(), port: 4001,
            sender_comp_id: "OMS".into(), target_comp_id: "IBKR".into(),
            password: "p".into(), ssl: true,
        }
    }

    #[test]
    fn a_disabled_connection_is_never_registered() {
        let c = conn("alpaca-paper", "DISABLED", CredentialState::Configured(alpaca()));
        assert!(matches!(classify(&c, true), ConnectionOutcome::Disabled));
        assert!(matches!(classify(&c, false), ConnectionOutcome::Disabled));
    }

    #[test]
    fn an_unconfigured_connection_is_reported_not_registered() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Unconfigured);
        assert!(matches!(classify(&c, false), ConnectionOutcome::Unconfigured));
    }

    /// An undecryptable credential must be surfaced, never silently skipped —
    /// the operator needs to know the difference between "not set up" and
    /// "set up but the key is wrong".
    #[test]
    fn an_error_credential_is_reported_with_its_reason() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Error("bad key".into()));
        match classify(&c, false) {
            ConnectionOutcome::Failed(msg) => assert!(msg.contains("bad key")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The claim this whole plan rests on: a REST credential can be swapped in a
    /// running process, a FIX one cannot, because the session thread parks
    /// forever with no stop path.
    #[test]
    fn fix_connections_need_a_restart_but_rest_ones_do_not() {
        let rest = conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca()));
        let fix = conn("ibkr-paper", "ACTIVE", CredentialState::Configured(ibkr()));

        // At boot both are registered — nothing is running yet to conflict with.
        assert!(matches!(classify(&rest, true), ConnectionOutcome::Registered));
        assert!(matches!(classify(&fix, true), ConnectionOutcome::Registered));

        // On reload the FIX one must report a restart rather than starting a
        // second session to the same venue.
        assert!(matches!(classify(&rest, false), ConnectionOutcome::Registered));
        assert!(matches!(classify(&fix, false), ConnectionOutcome::RestartRequired));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test reload::tests`
Expected: FAIL to compile.

- [ ] **Step 3: Write `classify` and the outcome types**

Above the tests in `src/reload.rs`:

```rust
//! Turning the credential store into live adapters — at boot and again on demand.
//!
//! Boot and reload share this module so a credential registered at startup and
//! one registered at runtime cannot diverge. The split that matters is which
//! connections can be rebuilt in a running process at all: a REST adapter is an
//! HTTP client and swapping it is free, while a FIX session owns an OS thread
//! that parks forever with no stop path (`fix::start_session`), so replacing one
//! would mean a second session logging in against the first.

use crate::credentials::{BrokerCredentials, Connection, CredentialState};

/// What happened to one connection during a registration pass.
///
/// `Serialize` because this is an HTTP response body in Task 5. Serde's default
/// external tagging gives `"Registered"` / `"RestartRequired"` for the unit
/// variants and `{"Failed": "..."}` for the one carrying a reason, which is what
/// the cockpit will match on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum ConnectionOutcome {
    Registered,
    /// The row exists but holds no credential — "needs setup".
    Unconfigured,
    /// `status <> 'ACTIVE'`; deliberately not registered.
    Disabled,
    /// Stored but unusable, or the adapter would not build. Carries the reason.
    Failed(String),
    /// Configured and valid, but changing it needs a process restart. Only FIX.
    RestartRequired,
}

/// Decide what to do with one connection.
///
/// `is_boot` is the whole reason this is a function rather than a match inline:
/// at boot a FIX connection is registered because nothing is running yet, while
/// on reload the same connection must report `RestartRequired` instead.
pub fn classify(conn: &Connection<BrokerCredentials>, is_boot: bool) -> ConnectionOutcome {
    if conn.status != "ACTIVE" {
        return ConnectionOutcome::Disabled;
    }
    match &conn.credentials {
        CredentialState::Unconfigured => ConnectionOutcome::Unconfigured,
        CredentialState::Error(e) => ConnectionOutcome::Failed(e.clone()),
        CredentialState::Configured(c) => match c {
            BrokerCredentials::Alpaca { .. } => ConnectionOutcome::Registered,
            BrokerCredentials::IbkrFix { .. } | BrokerCredentials::BinanceFix { .. } => {
                if is_boot {
                    ConnectionOutcome::Registered
                } else {
                    ConnectionOutcome::RestartRequired
                }
            }
        },
    }
}
```

**Note for the implementer:** `BinanceFix` covers both transports — the REST path is selected at registration time by `Transport::from_env`. Treating it as `RestartRequired` on reload is deliberately conservative: a Binance credential change needs a restart even on REST. Say so in a comment. Narrowing that is a follow-up, not this task.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test reload::tests` — expect 4 passing.

- [ ] **Step 5: Move the registration loop**

Now extract the actual work. In `src/main.rs`, the loop at roughly `:814` builds `BrokerRegistry` from `broker_connections`. Move its body into:

```rust
pub async fn build_registry(
    connections: &[Connection<BrokerCredentials>],
    is_boot: bool,
    deps: &RegistrationDeps,
) -> (BrokerRegistry, ReloadReport)
```

`RegistrationDeps` is a small struct carrying what the FIX starters and adapters need — the pool, the stream-health registry, the optional Kafka client, the position-changed sender. Define it in `src/reload.rs` and build it once in `serve()`.

Rules for the move:
- **Behaviour at boot must not change.** `is_boot = true` must produce exactly the registry `serve()` builds today.
- On reload (`is_boot = false`), a `RestartRequired` connection must **carry its existing adapter forward** from the current registry rather than being dropped — otherwise a reload would disarm a working FIX session. Take the current registry as an argument for that.
- Keep the per-connection logging where it is, including the `conn.kind` vs credential-variant mismatch warning.

Have `serve()` call it and `swap_registry` the result, so boot itself goes through the new path.

- [ ] **Step 6: Verify boot is unchanged**

Run: `cargo build && cargo test`. Then start the server against the dev database and confirm the same three registration lines appear as before:

```
registered ALPACA/PAPER adapter code=alpaca-paper
registered BINANCE FIX adapter env="PAPER"
registered DATABENTO/OPRA feed code=databento-opra
```

Do **not** run against a database you do not own; use the compose Postgres or a throwaway.

- [ ] **Step 7: Commit**

```bash
git add src/reload.rs src/main.rs
git commit -m "refactor(reload): extract registration so boot and reload share it

classify() is pure and carries the plan's central claim: a REST adapter can
be rebuilt in a running process, a FIX session cannot, because its thread
parks forever with no stop path. At boot both register; on reload a FIX
connection reports RestartRequired and keeps its existing adapter rather
than starting a second session against the first."
```

---

### Task 3: Reloadable feeds

**Files:**
- Modify: `src/app_state.rs`, `src/reload.rs`, `src/main.rs`

**Interfaces:**
- Produces:
  - `pub struct StreamRegistry` — `code -> JoinHandle<()>`, with `insert`, `abort_and_remove`, `codes`
  - `AppState::streams(&self) -> &StreamRegistry`

- [ ] **Step 1: Write the failing test**

```rust
    /// Aborting a feed must actually stop it, and re-inserting under the same
    /// code must not leave the old task running — two Databento sessions would
    /// double-subscribe and double-count marks.
    #[tokio::test]
    async fn replacing_a_stream_aborts_the_previous_one() {
        let reg = StreamRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

        let first = tokio::spawn(async move {
            // Hold the sender until aborted.
            let _tx = tx;
            std::future::pending::<()>().await;
        });
        reg.insert("databento-opra", first);

        reg.abort_and_remove("databento-opra");

        // The sender was dropped with the task, so the channel closes.
        assert!(rx.recv().await.is_none(), "the previous task should have been aborted");
        assert!(reg.codes().is_empty());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test app_state::tests::replacing_a_stream`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `StreamRegistry`**

A `Mutex<HashMap<String, JoinHandle<()>>>` is right here — unlike the broker registry this is not on the order path, so a lock costs nothing.

```rust
/// Handles for the supervised feed tasks, so a credential change can stop one
/// and start a replacement.
///
/// `abort()` rather than a graceful stop is acceptable *for feeds specifically*:
/// `stream_supervisor` already treats a dropped stream as a disconnect to
/// reconnect from, so losing an in-flight quote to an abort is indistinguishable
/// from losing it to a network blip — which the system already tolerates. That
/// reasoning does not extend to broker sessions, which is why FIX is not
/// reloadable here.
pub struct StreamRegistry { /* … */ }
```

- [ ] **Step 4: Wire feeds through it**

In `serve()`, every `tokio::spawn(stream_supervisor::supervise(...))` for a *credentialed* feed (the Databento one) registers its handle under the connection code. The Binance and Bybit market-data feeds are not credential-driven — leave them alone and say so in a comment.

Then extend the reload path: for each `Configured` feed connection, abort any existing task under that code and spawn a fresh session built from the new credential.

- [ ] **Step 5: Verify**

Run: `cargo test`. Then, against the dev database, start the server, confirm the Databento feed registers, and confirm no second `DATABENTO/OPRA` supervisor line appears after a reload (Task 5 gives you the trigger; until then verify by calling the reload function from a temporary test binary, or defer this check to Task 5 and say so).

- [ ] **Step 6: Commit**

```bash
git add src/app_state.rs src/reload.rs src/main.rs
git commit -m "feat(reload): make credentialed feeds restartable

A Mutex map is fine here — feeds are not on the order path, so the lock is
free, unlike the broker registry. abort() is acceptable for feeds because
the supervisor already treats a dropped stream as a reconnect; that
reasoning does not extend to broker sessions."
```

---

### Task 4: The Alpaca execution stream must follow its adapter

**Files:**
- Modify: `src/reload.rs`, `src/main.rs`

`alpaca_stream::run` is spawned with its own copy of the key and secret (`src/main.rs:952`). Swapping the adapter without restarting that task leaves orders routing on a new credential while fills arrive on the old one — the exact split Plan 2's Task 7 fixed at boot, reintroduced at reload.

- [ ] **Step 1: Write the failing test**

The stream itself needs a network, so test the *decision*:

```rust
    /// Swapping an Alpaca adapter must also restart its execution stream, or
    /// orders route on the new credential while fills arrive on the old one.
    #[test]
    fn reloading_alpaca_also_restarts_its_execution_stream() {
        let c = conn("alpaca-paper", "ACTIVE", CredentialState::Configured(alpaca()));
        assert!(restarts_execution_stream(&classify(&c, false)));

        let disabled = conn("alpaca-paper", "DISABLED", CredentialState::Configured(alpaca()));
        assert!(!restarts_execution_stream(&classify(&disabled, false)));
    }
```

- [ ] **Step 2: Run it, watch it fail, then implement**

`restarts_execution_stream(&ConnectionOutcome) -> bool` is true only for `Registered`. Then, in the reload path, register the Alpaca execution stream's `JoinHandle` in the same `StreamRegistry` (under a distinct key such as `alpaca-paper:exec` so it cannot collide with a feed code) and abort-and-respawn it whenever the adapter is swapped.

- [ ] **Step 3: Commit**

```bash
git add src/reload.rs src/main.rs
git commit -m "fix(reload): restart the Alpaca execution stream with its adapter

Otherwise a reload routes orders on the new credential while fills keep
arriving on the old one — the same split Plan 2 closed at boot."
```

---

### Task 5: The reload endpoint

**Files:**
- Modify: `src/admin.rs`, `src/main.rs`, `src/reload.rs`

**Interfaces:**
- Produces: `POST /admin/connections/reload` → `200` with a per-connection report.

- [ ] **Step 1: Write the failing test**

Test the report's shape and — the part that matters — that it cannot carry a secret:

```rust
    /// The report is an HTTP response body. It must name connections and
    /// outcomes and nothing else: a reload that echoed a credential would undo
    /// the entire point of encrypting it.
    #[test]
    fn the_report_carries_no_credential_material() {
        let report = ReloadReport {
            connections: vec![
                ("alpaca-paper".into(), ConnectionOutcome::Registered),
                ("ibkr-paper".into(), ConnectionOutcome::RestartRequired),
                ("binance-paper".into(), ConnectionOutcome::Failed("could not decrypt".into())),
            ],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        for secret in ["SUPERSECRET", "BEGIN PRIVATE KEY", "api_key"] {
            assert!(!json.contains(secret), "{secret} in {json}");
        }
        assert!(json.contains("alpaca-paper") && json.contains("RestartRequired"));
    }
```

- [ ] **Step 2: Implement the handler**

```rust
/// Re-read the credential store and apply what can be applied without a restart.
///
/// Returns 200 even when some connections could not be reloaded: a partial
/// result is the normal case (a FIX credential always needs a restart), and a
/// non-2xx would make the cockpit treat an expected outcome as an error. The
/// per-connection outcomes carry the detail.
pub async fn reload_connections(State(state): State<AppState>) -> impl IntoResponse
```

It loads the store with the master key, calls `build_registry` with `is_boot = false`, swaps the result in, restarts the feeds and execution streams whose adapters changed, and returns the report.

**A failed load must leave the running registry alone** and return 500 — never swap in an empty registry, which would disarm every broker.

Register it on the admin router beside the other `/admin/broker-connections` routes, inside the existing auth layer.

- [ ] **Step 3: Verify end to end**

Against the dev database (compose Postgres or a throwaway — **never** one you do not own):

```bash
# with the server running
curl -s -X POST localhost:3001/admin/connections/reload \
     -H "Authorization: Bearer $OMS_ADMIN_PASSWORD" | jq
```

Expect a per-connection report. Then change a credential (`oms config import-env` with a different value, or a direct `save_broker`), reload again, and confirm the Alpaca outcome is `Registered` while a FIX one is `RestartRequired`.

- [ ] **Step 4: Commit**

```bash
git add src/admin.rs src/main.rs src/reload.rs
git commit -m "feat(admin): POST /admin/connections/reload

200 even on a partial reload — a FIX credential always needs a restart, and
a non-2xx would make the cockpit treat the expected case as an error. A
failed load leaves the running registry in place rather than swapping in an
empty one."
```

---

### Task 6: Documentation

**Files:** `readme.md`

- [ ] **Step 1: Document what reloads and what does not**

Add to the credentials section: after changing a credential, `POST /admin/connections/reload` applies it. Alpaca and the Databento feed apply immediately; **IBKR and Binance FIX sessions need a process restart**, and the reload response says so per connection. State the reason in one sentence — a FIX session owns a thread with no stop path, and a second session would collide with the first.

Verify every claim against `src/reload.rs` before writing it.

- [ ] **Step 2: Commit**

```bash
git add readme.md
git commit -m "docs: what a credential reload does and does not cover"
```

---

## Verification

```bash
cargo build          # no new warnings
cargo test           # 237 existing + ~9 new
```

End-to-end, against the compose Postgres:

1. Start the server; confirm the same registration lines as before this plan.
2. `POST /admin/connections/reload` → a report naming every connection.
3. Change the Alpaca credential, reload, confirm `Registered` and that the execution stream restarted.
4. Confirm a FIX connection reports `RestartRequired` and that its existing adapter still works afterwards.
5. Break the master key, reload, confirm 500 and that the previous registry is still serving.

## Out of scope

- **Stopping FIX sessions.** Deliberate, per the constraint above. Making them reloadable means giving `fix::start_session`'s thread a shutdown path instead of `loop { park() }`, and handling logout and sequence numbers. Its own project.
- **Plan 4** — the credential write endpoints and cockpit screens that will call this reload.
- **Test-before-save.** The spec puts the connection test on the save path, which belongs with the write endpoint in Plan 4.
- **Kafka and OpenFIGI credentials**, still environment-only.
