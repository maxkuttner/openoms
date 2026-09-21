# Desktop shell — manual verification checklist

These are the items no agent in this session could verify: the sandbox has no
Accessibility or Screen Recording permission, no tool drives a native
non-Chrome window, and no agent types a password into a login form. Each item
below is a real gap, not a formality — run through them on a real machine
before trusting a release build.

Record the platform you tested on, and mark anything you could not exercise
as untested rather than skipping it silently.

**Platform tested:** _(fill in — OS and version)_

## Desktop shell

1. **The connection page renders.** Launch the app with no saved server. The
   bundled page appears, with the address field and the connect button.

2. **The error line says something useful.** Enter, in turn: an address that
   is syntactically invalid; one that is well-formed but unreachable; one
   that is reachable but is not an OMS (a plain web server); one behind a
   self-signed certificate. Each has one exact expected line, so check the
   wording rather than just that the four differ:

   | Address | Expected line |
   |---|---|
   | not a URL, or `ftp://…`, or carrying a path/query/fragment/userinfo | `Enter a full address starting with https://` |
   | well-formed, nothing listening | `Can't reach that address` |
   | reachable, but not an OMS | `Reachable, but that doesn't look like an OMS` |
   | reachable, but slow to answer | `No response — the server may be starting up` |
   | self-signed certificate | `Secure connection failed` |

   The self-signed case is the one to watch — TLS-versus-unreachable is
   decided by string-matching the reqwest error chain for
   "tls"/"certificate"/"ssl", which is nobody's stable API and is covered by
   no test. A self-signed cert reported as "Can't reach that address" is the
   known failure mode.

3. **The button disables while probing** and re-enables after a failure.

4. **A good address navigates.** Enter the running OMS; the window goes to
   its `/trade/` app.

5. **Session auth survives the shell.** Sign in to Keycloak *inside the app
   window* and confirm you land back in the trade app authenticated. This is
   the whole premise of the thin-shell approach — that the cookie story is
   unchanged — and it has never been exercised in a real window.

6. **Submitting an order works.** This is the one that proves the CSRF
   origin check passes from a webview, not just from a browser tab — its
   failure mode is silent and total: every order rejected with 403, with
   nothing in the connection flow above hinting that it's coming.

7. **Relaunch goes straight there, or to login — record which.** Quit and
   reopen the app. The saved address should send you to the trade app
   without the connection page, but whether you land already authenticated
   or back at the identity provider's login page determines whether the
   cookie jar persists across restarts. Note which one you saw; both are
   plausible outcomes of a correct implementation, but only one is the
   deliberate design, and the answer isn't obvious in advance.

8. **The capability boundary holds.** Mostly closed already — it is now
   *proven by an automated test*, not merely argued: `the_remote_page_cannot_invoke_connect`
   in `desktop/src-tauri/src/lib.rs` asserts that a local-origin invoke of
   `connect` reaches the handler while a remote-origin one is refused by the
   ACL. If you still want to see it live, do **not** check
   `window.__TAURI__`: `withGlobalTauri` is unset, so that property is
   `undefined` on every page — including the working local one — and
   checking it would give a false pass. Evaluate this instead, on the remote
   page:

   ```js
   window.__TAURI_INTERNALS__.invoke('connect', { url: 'https://evil.example.com' })
   ```

   `__TAURI_INTERNALS__` *is* injected on the remote page unconditionally, so
   the thing that proves the boundary is the ACL refusal you get back — an
   error naming `not allowed on window "main"` — not the object's absence.

9. **A hand-edited store fails closed.** Edit `server.json` in the app config
   dir to a valid-but-wrong URL — a different `https://` host, and separately
   a `file://` path — and relaunch. The window must stay on the connection
   page, not navigate. This closed a Critical finding; confirm the fix
   behaves in a real window, not only in tests.

10. **"Change server…" works, in both build modes.** Choose it, confirm the
    connection page returns, enter a different address, confirm the window
    follows, and confirm the stored address was *replaced* rather than a
    second one accumulating. Known and already ruled on: this is correct in
    a release build. Under `cargo tauri dev` it previously dead-ended on a
    500 — that was fixed by resolving the connection page's URL at runtime
    rather than trusting a build-time asset path. If you're testing under
    `cargo tauri dev`, confirm you see the connection page, not a 500.

## Carried from the trader GUI branch

11. **A real order submit**, end to end, and the **409
    duplicate-idempotency-key path** — the one that must not report "Order
    sent" when nothing was sent. The submit itself is item 6 above; do both
    in one sitting, and treat that item's 403 and this item's 409 as separate
    outcomes to look for rather than one pass/fail.

12. **Cancel shows "cancelling…"** and settles.

13. **The admin cockpit still renders its Blotter.** `InstrumentSelect` and
    `OrderTimeline` were rewired to take `apiGet` as a required prop. Static
    checks are clean — `tsc --noEmit` exits 0, both components are still
    mounted at `cockpit/src/pages/Blotter.tsx:73` and `:97` and
    `cockpit/src/components/CrudResource.tsx:47`, each passing
    `apiGet={api.get}`, and `OrderTimeline`'s `eventsPath` still defaults to
    `/admin/orders` — but the cockpit is behind an admin sign-in, so the
    runtime render is unconfirmed.

## Known-open, out of scope for this plan

- `CancelRejected` is modelled and rendered but nothing ever produces it: a
  broker refusing a cancel is not recorded. Backend work, unscoped.
- TLS-vs-unreachable string matching (see item 2). A robust fix is
  downcasting the source chain to `rustls::Error` plus an integration test
  against a local self-signed server.
