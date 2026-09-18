# Trader Desktop Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A desktop window that shows the existing trader app at `{server}/trade/`, plus a first-run screen asking which server.

**Architecture:** A Tauri v2 app in `desktop/`, kept out of the Cargo workspace. It bundles one static HTML page for entering a server URL, validates and probes that URL, stores it, and navigates the window to the remote app. Because the webview navigates to the OMS origin, the session cookie, the CSRF origin check and the whole OIDC flow work unchanged — no change to the web app or the OMS.

**Tech Stack:** Tauri 2.11 (current stable, released 2026-07-01), Rust, plain HTML/CSS. No Node, no bundler, no framework in the desktop app.

**Spec:** `docs/superpowers/specs/2026-09-18-trader-desktop-shell-design.md`

## Global Constraints

- **The shell is a window.** No notifications, no global hotkey, no always-on-top, no window-state persistence, no offline behaviour. Those are explicitly out of scope until the wrapper proves worth having.
- **No change to the web app or the OMS.** Nothing outside `desktop/`, plus the workspace `exclude`, a `.gitignore` line, and a README section.
- **No Node in the desktop app.** `frontendDist` points at a static directory; there is no dev command and no bundler.
- **`desktop/src-tauri` is excluded from the Cargo workspace**, so the root `cargo test` never builds a webview dependency tree.
- **The remote origin gets no Tauri command access.** The `connect` command is reachable only from the bundled local page. The app points wherever the user typed; whatever loads there must not reach into the shell.
- **A URL is stored only after it validates AND probes successfully.** Scheme must be `http://` or `https://`; `GET {url}/health` must answer 200 within 5 seconds.
- **Every failure gets its own message** (the five in the spec's table). "It didn't work" on a connection screen is how desktop apps become infuriating.
- Target macOS and Linux. Windows is untested, not prevented.

## File Structure

| File | Responsibility |
| --- | --- |
| `desktop/src-tauri/Cargo.toml` | Crate manifest; not a workspace member |
| `desktop/src-tauri/tauri.conf.json` | Window, `frontendDist`, bundle identifier |
| `desktop/src-tauri/src/main.rs` | Entry point; delegates to `lib.rs` |
| `desktop/src-tauri/src/lib.rs` | Builder, menu, first-run routing, the `connect` command |
| `desktop/src-tauri/src/server.rs` | **The testable core:** normalise, validate, probe, classify |
| `desktop/src-tauri/src/store.rs` | Read and write the stored URL |
| `desktop/src-tauri/capabilities/default.json` | Capability scoping — local page only |
| `desktop/ui/index.html` | The connection page: one input, one button, one error line |
| `Cargo.toml` (modify) | Add `desktop/src-tauri` to `exclude` |
| `.gitignore` (modify) | `desktop/src-tauri/target/` |
| `readme.md` (modify) | How to build and run it |

**A note on API signatures.** Tauri v2's API moved considerably from v1. Where this plan shows Tauri calls — window navigation, the path resolver, the menu API — **verify each against the installed 2.11 docs before assuming the snippet compiles**. The logic and structure are the requirement; the exact method names are not something to take on faith from this document. `server.rs` and `store.rs` are ordinary Rust and carry no such caveat.

---

### Task 1: Scaffold a window that shows the page

**Files:**
- Create: `desktop/src-tauri/Cargo.toml`, `desktop/src-tauri/tauri.conf.json`, `desktop/src-tauri/src/main.rs`, `desktop/src-tauri/src/lib.rs`, `desktop/ui/index.html`
- Modify: `Cargo.toml` (root), `.gitignore`

**Interfaces:**
- Consumes: nothing.
- Produces: a runnable Tauri app. Later tasks add the command, storage and menu to `lib.rs`.

- [ ] **Step 1: Exclude the crate from the workspace first**

Do this before creating the crate, or the next `cargo` command at the root fails with "file found in workspace directory but not listed". In the root `Cargo.toml`:

```toml
[workspace]
members = [".", "crates/symbology", "crates/dataprovider"]
# The desktop shell is a Tauri app with a webview dependency tree of its own.
# The OMS build already needs cmake and OpenSSL for the C++ FIX engine; pulling
# webview crates into every root `cargo test` would slow the main development
# loop for no benefit. It builds from desktop/ when asked, and never otherwise.
exclude = ["desktop/src-tauri"]
resolver = "2"
```

- [ ] **Step 2: Verify the root build is untouched**

Run: `cargo test --bin oms`
Expected: 414 passed, exactly as before. If the count moved or the build pulled new crates, the exclude is wrong — fix it before continuing.

- [ ] **Step 3: Scaffold the crate**

`desktop/src-tauri/Cargo.toml`:

```toml
[package]
name = "oms-trader-desktop"
version = "0.1.0"
edition = "2021"

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = [] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
```

`reqwest` is for the `/health` probe. `rustls-tls` rather than the default native-TLS so the desktop build does not acquire the same OpenSSL dependency the OMS has.

`desktop/src-tauri/tauri.conf.json`: `frontendDist` points at `../ui`, with no `beforeDevCommand` and no `devUrl` — there is no bundler. Set a window title of "openOMS Trade", a sensible minimum size (a trading screen below roughly 900×600 is unusable), and a bundle identifier such as `com.openoms.trader`.

`desktop/ui/index.html`: a single page — a heading, a text input for the server URL, a Connect button, and an empty element for the error line. Plain CSS inline. Match the OMS's own palette so it does not look like a different product: near-black `#0d1014` ground, `#eff1f4` text, `#22ae6c` for the button, JetBrains Mono if available with a monospace fallback.

`main.rs` delegates to `lib.rs`; `lib.rs` builds the app and shows the window.

- [ ] **Step 4: Verify it runs**

Run: `cd desktop/src-tauri && cargo tauri dev`
Expected: a window opens showing the connection page. Nothing is wired up yet — the button does nothing.

If the Tauri CLI is absent, install it (`cargo install tauri-cli --version '^2'`) and record in your report which version you used.

- [ ] **Step 5: Ignore the build directory and commit**

Add `desktop/src-tauri/target/` to `.gitignore`.

```bash
git add Cargo.toml .gitignore desktop/
git commit -m "feat(desktop): a Tauri window showing the connection page"
```

---

### Task 2: The server module — validate, probe, classify

**Files:**
- Create: `desktop/src-tauri/src/server.rs`
- Modify: `desktop/src-tauri/src/lib.rs` (add `mod server;`)
- Test: in-module `#[cfg(test)]` in `server.rs`

**Interfaces:**
- Produces:
  - `pub fn normalise(raw: &str) -> Result<String, ConnectError>`
  - `pub enum ConnectError { BadScheme, Unreachable, Tls, NotAnOms, Timeout }` with `pub fn message(&self) -> &'static str`
  - `pub fn classify(result: Result<u16, ProbeFailure>) -> Result<(), ConnectError>`
  - `pub enum ProbeFailure { Timeout, Tls, Connect }`
  - `pub async fn probe(url: &str, http: &dyn Probe) -> Result<(), ConnectError>` where `pub trait Probe` performs the request

**This is the whole testable surface of the project.** Everything else is a window and a webview. The `Probe` trait is what makes it testable: with the HTTP call injected, classification is exercised with no network, the same seam the OIDC token validator uses for its keys.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_host_and_port_is_accepted_and_left_alone() {
        assert_eq!(normalise("https://oms.example.com").unwrap(), "https://oms.example.com");
        assert_eq!(normalise("http://localhost:3001").unwrap(), "http://localhost:3001");
    }

    #[test]
    fn surrounding_whitespace_and_a_trailing_slash_are_removed() {
        // Both are what a human produces when pasting from a browser bar.
        assert_eq!(normalise("  https://oms.example.com/  ").unwrap(), "https://oms.example.com");
        assert_eq!(normalise("https://oms.example.com///").unwrap(), "https://oms.example.com");
    }

    #[test]
    fn a_missing_or_wrong_scheme_is_refused() {
        // Refused rather than guessed: silently prefixing https:// would send a
        // session cookie somewhere the user did not name.
        assert!(matches!(normalise("oms.example.com"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("ftp://oms.example.com"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("file:///etc/passwd"), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise(""), Err(ConnectError::BadScheme)));
        assert!(matches!(normalise("   "), Err(ConnectError::BadScheme)));
    }

    #[test]
    fn every_failure_says_something_different() {
        // The point of the enum: five distinct causes, five distinct messages.
        // A trader who cannot tell a typo from a dead server wastes an afternoon.
        let all = [
            ConnectError::BadScheme,
            ConnectError::Unreachable,
            ConnectError::Tls,
            ConnectError::NotAnOms,
            ConnectError::Timeout,
        ];
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            assert!(!e.message().is_empty(), "{e:?} has no message");
            assert!(seen.insert(e.message()), "{e:?} reuses another variant's message");
        }
    }

    #[test]
    fn a_200_is_the_only_success() {
        assert!(classify(Ok(200)).is_ok());
        assert!(matches!(classify(Ok(404)), Err(ConnectError::NotAnOms)));
        assert!(matches!(classify(Ok(500)), Err(ConnectError::NotAnOms)));
        // A 401 means something is there and answering, but /health is
        // unauthenticated on a real OMS — so this is not one.
        assert!(matches!(classify(Ok(401)), Err(ConnectError::NotAnOms)));
    }

    #[test]
    fn transport_failures_keep_their_identity() {
        assert!(matches!(classify(Err(ProbeFailure::Timeout)), Err(ConnectError::Timeout)));
        assert!(matches!(classify(Err(ProbeFailure::Tls)), Err(ConnectError::Tls)));
        assert!(matches!(classify(Err(ProbeFailure::Connect)), Err(ConnectError::Unreachable)));
    }

    #[tokio::test]
    async fn probe_reports_what_the_injected_client_saw() {
        struct Always(Result<u16, ProbeFailure>);
        #[async_trait::async_trait]
        impl Probe for Always {
            async fn get_status(&self, _url: &str) -> Result<u16, ProbeFailure> {
                match &self.0 {
                    Ok(s) => Ok(*s),
                    Err(ProbeFailure::Timeout) => Err(ProbeFailure::Timeout),
                    Err(ProbeFailure::Tls) => Err(ProbeFailure::Tls),
                    Err(ProbeFailure::Connect) => Err(ProbeFailure::Connect),
                }
            }
        }

        assert!(probe("https://oms.example.com", &Always(Ok(200))).await.is_ok());
        assert!(matches!(
            probe("https://oms.example.com", &Always(Err(ProbeFailure::Timeout))).await,
            Err(ConnectError::Timeout)
        ));
    }

    #[tokio::test]
    async fn the_probe_asks_for_health_on_the_given_base() {
        // The URL the probe builds is the contract with the server; a typo here
        // turns every connection attempt into "that doesn't look like an OMS".
        struct Recorder(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Probe for Recorder {
            async fn get_status(&self, url: &str) -> Result<u16, ProbeFailure> {
                self.0.lock().unwrap().push(url.to_string());
                Ok(200)
            }
        }

        let rec = Recorder(Default::default());
        probe("https://oms.example.com", &rec).await.unwrap();
        assert_eq!(rec.0.lock().unwrap().as_slice(), ["https://oms.example.com/health"]);
    }
}
```

Add `tokio` (with `macros` and `rt`) and `async-trait` as dev-dependencies, or as dependencies if the async trait is needed in the non-test build.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd desktop/src-tauri && cargo test`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement**

`normalise` trims, rejects anything not beginning `http://` or `https://`, and strips trailing slashes. **It never guesses a scheme** — prefixing `https://` onto a bare hostname would point a session cookie somewhere the user did not name.

`ConnectError::message` returns the five strings from the spec verbatim:

| Variant | Message |
| --- | --- |
| `BadScheme` | `Enter a full address starting with https://` |
| `Unreachable` | `Can't reach that address` |
| `Tls` | `Secure connection failed` |
| `NotAnOms` | `Reachable, but that doesn't look like an OMS` |
| `Timeout` | `No response — the server may be starting up` |

`probe` builds `{url}/health`, calls the injected `Probe`, and hands the result to `classify`. The real implementation of `Probe` wraps `reqwest` with a **5 second** timeout and maps its error kinds onto `ProbeFailure`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd desktop/src-tauri && cargo test`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add desktop/src-tauri
git commit -m "feat(desktop): validate, probe and classify a server address"
```

---

### Task 3: Storage, the connect command, and first-run routing

**Files:**
- Create: `desktop/src-tauri/src/store.rs`
- Modify: `desktop/src-tauri/src/lib.rs`, `desktop/ui/index.html`
- Test: in-module `#[cfg(test)]` in `store.rs`

**Interfaces:**
- Consumes: `server::{normalise, probe, ConnectError}`.
- Produces:
  - `pub fn load(dir: &Path) -> Option<String>`
  - `pub fn save(dir: &Path, url: &str) -> std::io::Result<()>`
  - the `connect` Tauri command, returning `Result<(), String>` where the error is `ConnectError::message()`

- [ ] **Step 1: Write the failing storage tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_url_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "https://oms.example.com").unwrap();
        assert_eq!(load(dir.path()).as_deref(), Some("https://oms.example.com"));
    }

    #[test]
    fn nothing_saved_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
    }

    #[test]
    fn a_later_save_replaces_the_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "https://one.example.com").unwrap();
        save(dir.path(), "https://two.example.com").unwrap();
        assert_eq!(load(dir.path()).as_deref(), Some("https://two.example.com"));
    }

    #[test]
    fn a_corrupt_file_reads_as_nothing_rather_than_panicking() {
        // A half-written or hand-edited file must send the user to the
        // connection page, not crash the app on launch.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.json"), b"{ not json").unwrap();
        assert_eq!(load(dir.path()), None);
    }
}
```

Add `tempfile` as a dev-dependency.

- [ ] **Step 2: Run to verify they fail**

Run: `cd desktop/src-tauri && cargo test`
Expected: FAIL — `store` does not exist.

- [ ] **Step 3: Implement storage**

A `server.json` holding one field, read and written with `serde_json`. `load` returns `None` for a missing, unreadable or malformed file — never an error, and never a panic: the only sensible response to an unreadable setting is to ask again.

- [ ] **Step 4: Wire the command and first-run routing**

In `lib.rs`:

- A `connect(url: String)` command that runs `normalise`, then `probe`, then `save`, then navigates the main window to `{url}/trade/`. Errors return `ConnectError::message()` as a `String`, which the page displays.
- On setup, `load` from the app-config directory (`app.path().app_config_dir()`), and if a URL is present navigate straight to `{url}/trade/`; otherwise leave the bundled page showing.

**Confirm the navigation and path-resolver APIs against the installed Tauri 2.11 docs** — they differ from v1, and this plan's names are not authoritative.

In `index.html`: on Connect, invoke the command, show the returned message in the error line on failure, and disable the button while in flight so a double-click cannot fire two probes.

- [ ] **Step 5: Verify by hand**

Run: `cd desktop/src-tauri && cargo tauri dev`, then in the window:

| Input | Expected |
| --- | --- |
| `oms.example.com` | "Enter a full address starting with https://" |
| `https://nope.invalid` | "Can't reach that address" |
| `http://localhost:9999` | "Can't reach that address" |
| a running OMS URL | window navigates to the trade app |

Then quit and relaunch: it should go straight to the app without asking. Record each result in your report.

To have a real OMS to point at, start one against the local docker Postgres:
`POSTGRES_HOST=localhost POSTGRES_PORT=5432 POSTGRES_USERNAME=postgres POSTGRES_PASSWORD=postgres POSTGRES_DATABASE=ods cargo run` from the repository root. It binds `:3001` and takes roughly 40–55 seconds to come up. Note that `/trade/` only exists when an `[auth.oidc]` block is configured; without one the window will land on a 404, which is itself worth recording.

- [ ] **Step 6: Commit**

```bash
git add desktop/
git commit -m "feat(desktop): store a validated server and open the trade app"
```

---

### Task 4: Capability scoping and the menu

**Files:**
- Create: `desktop/src-tauri/capabilities/default.json`
- Modify: `desktop/src-tauri/src/lib.rs`, `desktop/src-tauri/tauri.conf.json`

**Interfaces:**
- Consumes: the `connect` command from Task 3.
- Produces: a "Change server…" menu item that navigates back to the bundled page.

**The security requirement of this project lives here.** The shell points at a URL the user typed. Whatever loads there must not be able to call `connect`, touch the filesystem, or change where the app points next.

- [ ] **Step 1: Scope capabilities to the local page**

Write `capabilities/default.json` granting the `connect` command **only** to the bundled local window context, with no remote URL in scope. Tauri v2 grants nothing to remote origins by default, so the requirement is as much "do not widen this" as "narrow it": do not add a `remote` block, and do not add filesystem, shell or process permissions.

- [ ] **Step 2: Verify the remote origin really has no access**

With the app connected to a running OMS, open the webview's developer console on the remote page and evaluate:

```js
window.__TAURI__
```

Expected: `undefined`, or present with no invokable commands. Then attempt `window.__TAURI__?.core?.invoke?.('connect', { url: 'https://evil.example.com' })` and confirm it does not run. **Record the actual output in your report** — this is the one check that proves the boundary, and a plausible-looking config that grants access anyway would be invisible otherwise.

If developer tools are unavailable in a release build, verify in `cargo tauri dev` and say so.

- [ ] **Step 3: Add the menu item**

A native menu with "Change server…" that navigates the window back to the bundled page. Without it, a wrong-but-reachable URL leaves a trader with no way out except deleting a file. Confirm the menu API against the Tauri 2.11 docs.

- [ ] **Step 4: Verify by hand**

Connect to a server, choose "Change server…", confirm the connection page returns, enter a different URL, and confirm the window follows it. Confirm the previously stored URL is replaced rather than accumulating.

- [ ] **Step 5: Commit**

```bash
git add desktop/
git commit -m "feat(desktop): keep the remote page out of the shell, and let the server change"
```

---

### Task 5: Documentation and the platform checklist

**Files:**
- Modify: `readme.md`

- [ ] **Step 1: README**

Add a short section after the trade app's: the desktop shell lives in `desktop/`, builds with `cargo tauri dev` / `cargo tauri build`, asks for a server on first run, and is a window around the same web app rather than a separate client. State plainly that **login happens inside the webview**, so an identity provider that blocks embedded webviews will not work — Keycloak is fine, Google is not. The file is `readme.md`, lowercase.

Match the tone of the surrounding sections; read them first.

- [ ] **Step 2: Write the platform checklist into your report**

You are not expected to execute what you cannot: record which platform you tested on and mark the rest untested.

1. First launch with no stored URL shows the connection page.
2. Each bad-address case produces its own specific message.
3. A good address stores, navigates, and lands on the trade app.
4. Login completes inside the window and returns to `/trade/`.
5. **Submitting an order works** — this is the one that proves the CSRF origin check passes from a webview, and its failure mode is every order rejected with 403.
6. Quitting and relaunching goes straight to the app, or to login — **record which**, since it determines whether the cookie jar persists.
7. "Change server…" returns to the connection page and a new URL takes effect.
8. `window.__TAURI__` is unreachable from the remote page.

- [ ] **Step 3: Verify the root build is still untouched**

Run: `cargo test --bin oms` from the repository root.
Expected: 414 passed. The desktop app must not have leaked into the OMS build at any point.

- [ ] **Step 4: Commit**

```bash
git add readme.md
git commit -m "docs: the desktop shell, and what it needs from an identity provider"
```

## Self-Review

**Spec coverage.** Layout and workspace exclusion → Task 1. Connection flow, validation, the five messages, storage → Tasks 2 and 3. Getting back via the menu → Task 4. Capability scoping → Task 4. The three things to verify early: cookie persistence → Task 5 checklist item 6; the CSRF origin check → item 5; embedded-webview login → item 4 and the README. Testing split → Tasks 2 and 3 for the pure core, Task 5 for the manual checklist.

**Gap found and closed while reviewing:** the spec says a stored URL is used on launch, but says nothing about what happens when that file is corrupt or half-written. A crash on launch with no way to reach the connection page would be unrecoverable without deleting a file the user cannot find. Task 3 now requires `load` to return `None` for a malformed file, with a test.

**Second gap:** nothing verified that the probe requests the right path. A typo building `{url}/health` would turn every attempt into "that doesn't look like an OMS" and look like a server problem. Task 2 gained a test asserting the exact URL requested.

**Type consistency.** `ConnectError` and `ProbeFailure` are defined in Task 2 and consumed in Task 3. `load`/`save` are defined in Task 3 and used in the same task's setup hook. The `Probe` trait exists only so the real `reqwest` implementation and the test doubles share a shape.

**Known risk.** This plan names Tauri v2 APIs — window navigation, the path resolver, the menu — that moved from v1 and that I have not compiled against 2.11. The structure and the logic are the requirement; every Tauri call should be checked against the installed version's documentation rather than trusted from this document. `server.rs` and `store.rs` are plain Rust and carry no such risk, which is also where all the tests are.
