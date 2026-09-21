# A desktop shell for the trader app

**Date:** 2026-09-18
**Status:** approved design, not yet implemented
**Depends on:** `2026-09-17-trader-gui-design.md` — this wraps the app built there.
**Branched from:** `feat/trader-gui`, which is unmerged at the time of writing.

## Problem

The trader app lives at `/trade/` in a browser tab. A trading screen in a tab is
a tab: it loses its window position, it sits behind whatever else is open, and it
looks like a website rather than a desk tool.

The cheapest honest answer is a desktop window pointed at the same app.

## What we are building

A Tauri v2 application whose window shows `{server}/trade/`, plus a first-run
screen asking which server. Nothing else.

**It is a window, and the design does not pretend otherwise.** No notifications, no
global hotkey, no always-on-top, no offline behaviour, no local state. Those are
the things that would make a wrapper worth more than a browser tab, and they are
deliberately out of scope until the wrapper itself proves worth having.

### Why this works at all: the auth model is untouched

Everything the trader app does rests on a **same-origin httpOnly session cookie**.
Because this shell navigates the webview *to the OMS origin*, all of it continues
to work unchanged:

1. The webview loads `{server}/trade/`.
2. The app calls `/auth/me`, gets 401, and sets `window.location` to
   `/auth/login?return_to=/trade/`.
3. The OMS 303s to the identity provider. **The trader signs in inside the
   webview.**
4. The callback sets the session cookie in the webview's cookie jar and returns to
   `/trade/`.

Not one line of the web app or the OMS changes. That is the entire argument for
this approach over a native client.

### The constraint that decides viability

**RFC 8252 discourages embedded user-agents for OAuth, and some identity providers
enforce it.** Google blocks embedded webviews outright; Okta and Entra can be
configured to; Keycloak does not.

If the provider a desk actually deploys blocks embedded webviews, **this design is
not viable** and the alternative is a real native client: a public OIDC client with
PKCE in the system browser, tokens in the OS keychain, and refresh handling — a
different auth model from the one deliberately chosen in the OIDC spec, and its own
project.

**Confirm this against the intended provider before implementation starts.** It does
not change the design; it decides whether the design is worth building.

## Out of scope

- **Notifications, global hotkeys, always-on-top, window-state persistence.** Add
  them once the wrapper has earned it.
- **Distribution.** Local builds only: no CI job, no release artifacts, no code
  signing or notarization. Adding a release matrix job later is additive.
- **Windows.** macOS and Linux only, matching the existing release targets. Nothing
  here prevents Windows; it is simply untested.
- **Streaming.** Deferred with the web app's own polling decision.
- **Any change to the web app or the OMS.**

## Design

### Layout and build

```
desktop/
  src-tauri/          Rust: the shell, the connect command, the menu
  ui/index.html       the connection page — plain HTML and inline CSS
```

**No Node in the desktop app.** Tauri v2 points `frontendDist` at a static
directory with no bundler and no dev command. The connection page is one input and
a button; bundling a framework to draw it would be absurd. The toolchain is the
Tauri CLI plus Rust.

This also keeps the desktop app independent of `cockpit/`'s build — which matters,
because the app will routinely point at an OMS built from a different commit than
the one it was built from.

**Excluded from the Cargo workspace.** The root `Cargo.toml` lists
`members = [".", "crates/symbology", "crates/dataprovider"]`, and a nested crate
that is neither listed nor excluded is an error — so `desktop/src-tauri` goes in
`exclude`. This is deliberate: the OMS build already needs cmake and OpenSSL for the
C++ FIX engine, and pulling a webview dependency tree into every root `cargo test`
would slow the main development loop for no benefit.

Build with `cd desktop && cargo tauri dev` / `cargo tauri build`.
`.gitignore` gains `desktop/src-tauri/target/`.

### The connection flow

On launch the shell reads a stored URL from Tauri's app-config directory:

- **Present** → navigate the main window to `{url}/trade/`.
- **Absent** → show the bundled connection page.

**Connect** passes the typed value to one Tauri command, which:

1. normalises it — trim, drop a trailing slash;
2. requires an `http://` or `https://` scheme;
3. `GET`s `{url}/health` with a **5 second** timeout and requires 200;
4. stores the URL and navigates the window, **only** on success.

`/health` is unauthenticated and already returns `200 OK`, making it a clean
liveness probe.

### Error messages

A connection screen that says only "it didn't work" is where desktop apps become
infuriating. Each case gets its own message:

| Condition | Message |
| --- | --- |
| No scheme, or an unsupported one | "Enter a full address starting with https://" |
| DNS failure or connection refused | "Can't reach that address" |
| TLS failure | "Secure connection failed" |
| Non-200 from `/health` | "Reachable, but that doesn't look like an OMS" |
| Timeout | "No response — the server may be starting up" |

The TLS case is separate on purpose: a self-signed certificate in a test deployment
is a different problem from a wrong address, and conflating them wastes an
afternoon. The timeout message acknowledges that the OMS takes roughly 40 seconds
to boot.

### Storage

**A JSON file in the app-config directory, not `localStorage`.** `localStorage`
belongs to the webview's *origin*, so it would live under whichever server the app
last pointed at, and would vanish or diverge the moment the URL changed. One string
in a file the shell owns is the right home.

### Getting back

A native menu item, **"Change server…"**, navigates the window back to the bundled
page. Without it, a wrong-but-reachable URL leaves a trader with no way out except
deleting a file.

### What the shell exposes to the remote page: nothing

Tauri v2 scopes capabilities per window and per origin. **The `connect` command is
granted only to the bundled local page; the remote OMS origin gets no Tauri command
access at all.**

This matters because the shell points at a URL the user typed. Whatever loads there
must not be able to call into the desktop app, read the filesystem, or change where
the app points next. The web app needs none of it, and withholding it means a
compromised or mistyped server cannot reach past the webview.

## Three things to verify early, not assume

These are not design decisions; they are facts about the platform that the design
depends on, and each has a failure mode worth knowing about before a user finds it.

1. **Cookie persistence across launches.** If the webview's cookie jar is not
   persistent, the trader re-authenticates on every launch. Possibly acceptable —
   the session's absolute cap is 12 hours and its idle timeout 30 minutes, so
   re-login is routine — but it is the difference between "opens instantly" and
   "always asks".
2. **The CSRF origin check.** Writes require an `Origin` header matching
   `public_base_url`. A same-origin `POST` from a webview should send it, but the
   two engines in scope are different implementations — WKWebView on macOS,
   WebKitGTK on Linux — and the failure mode is **every order submission rejected
   with 403**. Verify on both.
3. **Embedded-webview login** against the intended identity provider. See above.

## Testing

Most of this is not unit-testable, and the spec does not pretend otherwise by
specifying tests that assert nothing.

**Unit-testable, and where the effort goes:**

- URL normalisation and scheme validation: trailing slashes, missing scheme,
  `ftp://`, an empty string, whitespace, a bare hostname.
- Classification of a probe result into the five error messages above.

**The probe must take its HTTP call as an injectable dependency**, so the
classification logic is testable with no network. This mirrors the OIDC token
validator, which takes its keys as an argument for the same reason: the seam is what
makes the difference between a real test and a mock of yourself.

**Manual checklist**, run per platform:

1. First launch with no stored URL shows the connection page.
2. A bad address produces the *specific* message for that failure, not a generic one.
3. A good address stores, navigates, and lands on the trade app.
4. Login completes inside the window and returns to `/trade/`.
5. Submitting an order works — specifically confirming the CSRF origin check passes.
6. Quitting and relaunching goes straight to the app (or to login, depending on
   cookie persistence — note which).
7. "Change server…" returns to the connection page, and a new URL takes effect.
