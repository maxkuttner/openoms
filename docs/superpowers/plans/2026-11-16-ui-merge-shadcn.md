# One App on shadcn/ui Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `/cockpit/` and `/trade/` SPAs with one app at `/app/`, an Open WebUI-style shell (same sidebar for everyone, Admin Panel behind the user menu for admins), and swap Mantine for shadcn/ui + Tailwind in the approved dark graphite look. Feature-for-feature port.

**Architecture:** One Vite entry (`cockpit/index.html`) renders `AuthGate` → `Shell` (sidebar, top bar, user menu, ⌘K) → routes. Pure modules decide navigation (`nav.ts`) and access (`access.ts`) and are unit-tested with `node --test`; React components stay thin. One API client replaces the cockpit and trade clients. The Rust side serves one shell at `/app/` from the embedded `cockpit/dist`.

**Tech Stack:** React 18, TypeScript, Vite, Tailwind CSS v4 (`@tailwindcss/vite`), shadcn/ui (Radix), `cmdk`, `sonner`, `react-hook-form` + `zod` + `@hookform/resolvers`, `lucide-react`, `@fontsource/inter`, TanStack Query, React Router 7. Rust: axum, `include_dir!`.

**Spec:** `docs/superpowers/specs/2026-11-16-ui-merge-shadcn-design.md`

## Global Constraints

- Feature-for-feature port: no new screens, fields or endpoints. Every behaviour listed in the spec's anti-goals survives (forms, validation, confirmations, MFA prompt, forced password change, pending screen, setup checklist, task center, jobs indicator, stream-health strip, keyboard flows).
- Dark only. Colours only from the token table in the spec, defined once as CSS variables in `cockpit/src/styles/tokens.css`.
- Font Inter 13px base (bundled via `@fontsource/inter`, no CDN); numbers `ui-monospace, SFMono-Regular, Menlo, monospace` + `tabular-nums`, right-aligned in tables; table rows ≈ 26px; top bar 38px; sidebar 190px; radius 5px.
- App served at `/app/`; `/` redirects to `/app/`; `/cockpit/*` and `/trade/*` removed without redirects. Asset base stays `/ui/`.
- Pure modules (`crud`, `lifecycle`, `positions`, `refresh`, `strategies`, `streamHealth`, `venues`, `ibkrImport`, `liveUpdatesCore`, `login`, `checklist`, `fees`, `groups`, `users`) change only import paths; their tests stay green. New pure modules use relative imports (no `@/` alias) so `node --test` can load them.
- CSRF origin rule and the vite dev proxy unchanged.
- Each task leaves `npm test` and `npm run build` green. Mixed Mantine/shadcn is allowed only on this branch, never on `main`.

## Review Focus

1. **Non-admin opens an admin URL directly** (`/app/admin/users`): sees the in-shell "no access" message, the sidebar still works, no 403 toast storm. → test in Task 3 (`adminRouteAllowed`) and click-through in Task 9.
2. **Session expires mid-use** (401 on any call): one navigation to `/app/login?return_to=…`, not a loop or several. → test in Task 2 (`loginHref` for the single home) and the `navigatingToLogin` guard kept in the merged client.
3. **Deep link reload** (`/app/admin/broker-connections` refreshed): server serves the shell, router lands on the page. → Rust test in Task 8.
4. **OIDC user without an OMS account**: redirected to `/app/access?reason=no_account`, which renders without a session. → Rust test in Task 8, route test in Task 4 (`/access` and `/login` render outside the gate).
5. **⌘K for a non-admin** lists no admin pages. → test in Task 3 (`commandItems(false)`).

---

## File Structure

```
cockpit/
  index.html                 single entry (trade.html deleted in Task 8)
  components.json            shadcn config
  vite.config.ts             tailwind plugin, @ alias, one rollup input
  src/
    main.tsx                 providers, BrowserRouter basename
    styles/tokens.css        the ONLY colour list (replaces palette.ts)
    styles/globals.css       tailwind import, base, fonts, .num utility
    lib/utils.ts             shadcn cn()
    components/ui/*          shadcn-generated primitives
    access.ts (+test)        pure: session → AccessState
    nav.ts (+test)           pure: sidebar, admin tabs, ⌘K items
    api/client.ts            merged client (cockpit + trade)
    shell/AuthGate.tsx       checking/signed_out/pending/change_password/ready
    shell/Shell.tsx          sidebar + top bar + <Outlet/>
    shell/UserMenu.tsx       Settings · Admin Panel · Sign out
    shell/CommandMenu.tsx    ⌘K
    shell/AdminPanel.tsx     tabs + sub-nav + <Outlet/>, NoAccess
    shell/TopBar.tsx         title, chips, ConnectionDot, StreamHealthStrip
    login/*                  ported sign-in screens
    pages/trading/*          Trade, Positions, Access (moved from trade/pages)
    pages/admin/*            all former cockpit pages
    components/*             shared domain components (ported in place)
```

`src/trade/` disappears by Task 8; its components move to `src/components/trading/`.

---

### Task 1: Tooling, tokens, shadcn primitives

**Files:**
- Modify: `cockpit/package.json`, `cockpit/vite.config.ts`, `cockpit/tsconfig.json`, `cockpit/check-palette.sh`
- Create: `cockpit/components.json`, `cockpit/src/styles/tokens.css`, `cockpit/src/styles/globals.css`, `cockpit/src/lib/utils.ts`, `cockpit/src/components/ui/*`

**Interfaces:**
- Produces: Tailwind classes backed by tokens (`bg-background`, `bg-sidebar`, `text-foreground`, `text-foreground-2`, `text-muted-foreground`, `border-border`, `border-subtle`, `bg-accent`, `bg-primary text-primary-foreground`, `text-buy`, `text-sell`, `bg-buy`, `bg-sell`); utility class `.num` (monospace + tabular-nums); `cn(...classes)` from `@/lib/utils`; shadcn components under `@/components/ui/`: button, input, label, select, checkbox, switch, textarea, dialog, alert-dialog, dropdown-menu, popover, tooltip, tabs, table, badge, card, separator, scroll-area, skeleton, form, command, sonner, alert.

- [ ] **Step 1:** Add deps: `tailwindcss @tailwindcss/vite class-variance-authority clsx tailwind-merge lucide-react cmdk sonner react-hook-form zod @hookform/resolvers @fontsource/inter` plus the `@radix-ui/*` packages the shadcn CLI pulls. Keep Mantine for now.
- [ ] **Step 2:** `vite.config.ts`: add `tailwindcss()` plugin and `resolve.alias { "@": "/src" }`; `tsconfig.json`: `paths { "@/*": ["./src/*"] }`. Run `npx shadcn@latest init` (style new-york, base colour neutral, CSS `src/styles/globals.css`), then `npx shadcn@latest add` the list above.
- [ ] **Step 3:** Write `tokens.css` with the spec's hex values mapped to shadcn variable names (`--background:#111315; --sidebar:#0c0e10; --foreground:#d6d9dc; --foreground-2:#a3a8ad; --muted-foreground:#6c737a; --border:#22262a; --border-subtle:#1a1d20; --accent:#1d2125; --primary:#f2a93b; --primary-foreground:#111111; --buy:#26a69a; --sell:#ef5350; --destructive:#ef5350; --ring:#f2a93b; --radius:5px`), set on `:root` (dark only, no `.dark` variant). `globals.css`: import tokens, `@fontsource/inter/{400,500,600}.css`, `body { font: 13px Inter; background: var(--background) }`, `.num { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-variant-numeric: tabular-nums }`, `@theme inline` mapping for the extra tokens (`foreground-2`, `border-subtle`, `buy`, `sell`, `sidebar`).
- [ ] **Step 4:** `check-palette.sh`: the allowed file becomes `src/styles/tokens.css` (keep `src/palette.ts` allowed until Task 8 deletes it); message updated.
- [ ] **Step 5:** Verify: `cd cockpit && npm test && npm run build && sh check-palette.sh` all pass. (Mantine pages may look off on this branch; expected.)
- [ ] **Step 6:** Commit `chore(ui): tailwind, shadcn primitives and the graphite tokens`.

---

### Task 2: Merged API client and access state

**Files:**
- Create: `cockpit/src/access.ts`, `cockpit/src/access.test.ts`
- Modify: `cockpit/src/api/client.ts`, `cockpit/src/login/api.ts`
- Later deleted (Task 8): `cockpit/src/trade/api/client.ts`

**Interfaces:**
- Produces:
  - `type AccessState = "signed_out" | "pending" | "change_password" | "ready" | "unavailable"`
  - `accessFromMe(status: number, me: SessionInfo | null): AccessState` (in `access.ts`, pure)
  - `isAdmin(me: SessionInfo | null): boolean` → `me?.role === "admin"`
  - `APP_HOME: string` in `login/api.ts` = `import.meta.env.DEV ? "/" : "/app/"` (replaces `COCKPIT_HOME` and `TRADE_HOME`)
  - `api.get/post/put/patch/del` (unchanged signatures) plus everything `tradeApi` exported, merged into `api` in `api/client.ts`; `goToLogin()` uses `APP_HOME` and keeps the `navigatingToLogin` once-only guard; `onMfaRequired` unchanged.
  - `sessionAccess(): Promise<{ state: AccessState; me: SessionInfo | null }>` calling `GET /auth/me` (replaces `consoleAccess`, which probed `/admin/users`).

- [ ] **Step 1: Write failing tests** in `access.test.ts`:
```ts
test("401 is signed out", () => assert.equal(accessFromMe(401, null), "signed_out"));
test("5xx or network is unavailable", () => assert.equal(accessFromMe(502, null), "unavailable"));
test("pending role", () => assert.equal(accessFromMe(200, { auth_method: "local", role: "pending" }), "pending"));
test("must change password beats ready", () =>
  assert.equal(accessFromMe(200, { auth_method: "local", role: "user", must_change_password: true }), "change_password"));
test("plain user is ready", () => assert.equal(accessFromMe(200, { auth_method: "oidc", role: "user" }), "ready"));
test("admin is ready and admin", () => {
  const me = { auth_method: "local", role: "admin" };
  assert.equal(accessFromMe(200, me), "ready"); assert.equal(isAdmin(me), true);
});
test("user is not admin", () => assert.equal(isAdmin({ auth_method: "local", role: "user" }), false));
```
Check what `/auth/me` returns for a pending account today (it may be 403 `approval_pending` rather than 200 with role `pending`; `isApprovalPending` in `login.ts` handles the 403 body) and add the matching case: `accessFromMe(403, null, body)` — if so, extend the signature to `accessFromMe(status, me, body = "")` and test `"pending"` and `"change_password"` from 403 bodies too.
- [ ] **Step 2:** `node --test src/access.test.ts` → FAIL (module missing).
- [ ] **Step 3:** Implement `access.ts` (reuse `isPendingAccount`, `mustChangePassword`, `isApprovalPending`, `isPasswordChangeRequired` from `login.ts`). Merge `trade/api/client.ts` exports into `api/client.ts`; point `trade/*` imports at the merged client. Replace `COCKPIT_HOME`/`TRADE_HOME` with `APP_HOME` everywhere.
- [ ] **Step 4:** `npm test && npm run build` → PASS.
- [ ] **Step 5:** Commit `refactor(ui): one API client and a pure access state`.

---

### Task 3: Navigation model

**Files:**
- Create: `cockpit/src/nav.ts`, `cockpit/src/nav.test.ts`

**Interfaces:**
- Produces:
```ts
type Page = { id: string; title: string; path: string };           // path relative to the router base
type AdminTab = { id: "people" | "setup" | "connectivity" | "monitoring"; title: string; pages: Page[] };
export const SIDEBAR: Page[];          // trade "/", positions "/positions"
export const ACCOUNT_PAGES: Page[];    // settings "/settings", access "/access"
export const ADMIN_TABS: AdminTab[];
export function adminTabFor(path: string): AdminTab | null;
export function adminRouteAllowed(path: string, admin: boolean): boolean;
export function commandItems(admin: boolean): Page[];
```
Admin paths: `/admin/users`, `/admin/groups`, `/admin/sign-in`, `/admin/api`, `/admin/portfolios`, `/admin/accounts`, `/admin/strategies`, `/admin/risk-limits`, `/admin/instruments`, `/admin/broker-connections`, `/admin/data-feeds`, `/admin/blotter`. Tabs: People = Users, Groups, Sign-in, API; Trading setup = Portfolios, Accounts, Strategies, Risk limits, Instruments; Connectivity = Broker connections, Data feeds; Monitoring = Blotter.

- [ ] **Step 1: Write failing tests:**
```ts
test("sidebar is Trade and Positions", () => assert.deepEqual(SIDEBAR.map(p => p.title), ["Trade", "Positions"]));
test("admin tabs group the twelve admin pages", () => {
  assert.deepEqual(ADMIN_TABS.map(t => t.title), ["People", "Trading setup", "Connectivity", "Monitoring"]);
  assert.equal(ADMIN_TABS.flatMap(t => t.pages).length, 12);
});
test("tab lookup", () => {
  assert.equal(adminTabFor("/admin/data-feeds")?.id, "connectivity");
  assert.equal(adminTabFor("/positions"), null);
});
test("non-admin may not open admin routes", () => {
  assert.equal(adminRouteAllowed("/admin/users", false), false);
  assert.equal(adminRouteAllowed("/admin/users", true), true);
  assert.equal(adminRouteAllowed("/positions", false), true);
});
test("command menu hides admin pages from non-admins", () => {
  assert.ok(commandItems(false).every(p => !p.path.startsWith("/admin")));
  assert.equal(commandItems(true).filter(p => p.path.startsWith("/admin")).length, 12);
});
```
- [ ] **Step 2:** `node --test src/nav.test.ts` → FAIL.
- [ ] **Step 3:** Implement `nav.ts`.
- [ ] **Step 4:** → PASS.
- [ ] **Step 5:** Commit `feat(ui): navigation model for the merged app`.

---

### Task 4: Shell, auth gate, sign-in screens

**Files:**
- Create: `cockpit/src/shell/{AuthGate,Shell,TopBar,UserMenu,CommandMenu,AdminPanel}.tsx`, `cockpit/src/routes.tsx`
- Modify: `cockpit/src/main.tsx`, `cockpit/src/App.tsx` (becomes the router tree or is replaced by `routes.tsx`), `cockpit/src/login/{LoginPage,Frame,MfaPrompt,ChangePassword,Totp,AccountSettings}.tsx`, `cockpit/src/components/{Logo,StreamHealthStrip}.tsx`

**Interfaces:**
- Consumes: `sessionAccess`, `isAdmin`, `APP_HOME`, `onMfaRequired` (Task 2); `SIDEBAR`, `ADMIN_TABS`, `adminRouteAllowed`, `commandItems` (Task 3).
- Produces:
  - `useSession(): { me: SessionInfo; admin: boolean }` (context from `AuthGate`, for pages and menus).
  - Router basename `import.meta.env.DEV ? "/" : "/app/"`. Routes: `/login` and `/access` render **outside** `AuthGate`; inside: `/` Trade, `/positions`, `/settings` (AccountSettings), `/admin` → redirect `/admin/users`, `/admin/*` under `AdminPanel`; `*` → `/`.
  - `AdminPanel` renders `NoAccess` ("You don't have access to the Admin Panel.") when `!admin`, else tab bar (`ADMIN_TABS`) + sub-nav for the active tab + `<Outlet/>`; `SetupChecklist` banner sits above the tabs.
  - `TopBar` props `{ title: string; chips?: ReactNode }`; holds `ConnectionDot` (ported from `trade/App.tsx`, uses `--buy`/`--sell`/`--primary` for ok/error/checking) and `StreamHealthStrip`.
  - `CommandMenu`: `⌘K`/`Ctrl+K` toggles; items from `commandItems(admin)` plus "Sign out".
  - `<Toaster/>` from sonner mounted in `main.tsx`; `toast()` replaces `notifications.show()` everywhere from here on.

- [ ] **Step 1:** Build `main.tsx` without `MantineProvider` for the new tree, but keep `MantineProvider` wrapping it until Task 8 so not-yet-ported pages still render. Import `styles/globals.css`.
- [ ] **Step 2:** Implement `AuthGate`: `checking` → skeleton; `signed_out` → `goToLogin()`; `pending` → `PendingScreen`; `change_password` → `ForcedPasswordChange` (on done: refetch); `unavailable` → "The OMS isn't responding. Try again."; `ready` → provide session, mount `MfaPrompt` registered via `onMfaRequired`, `LiveUpdatesProvider`, then children.
- [ ] **Step 3:** Implement `Shell`, `UserMenu`, `CommandMenu`, `AdminPanel`, `TopBar` per the spec's shell sketch and tokens (sidebar 190px `bg-sidebar`, active item `bg-accent`, user menu at sidebar foot with display name).
- [ ] **Step 4:** Port the six `login/*` screens to shadcn (`Card`, `Input`, `Button`, `Form`), same copy, same steps (`nextStepFromLoginResponse`, TOTP enroll/confirm/disable, API keys in AccountSettings). Not-yet-ported pages are routed as-is (Mantine) so the app stays usable.
- [ ] **Step 5:** Verify: `npm test && npm run build`; `npm run dev` against a running OMS (`OMS_URL`), sign in as admin and as a user: sidebar identical, Admin Panel only in the admin's menu, `/admin/users` as user shows NoAccess, ⌘K lists match `commandItems`, login/MFA/forced-password/pending flows work.
- [ ] **Step 6:** Commit `feat(ui): merged shell, auth gate and sign-in screens on shadcn`.

---

### Task 5: Trading pages

**Files:**
- Move + port: `cockpit/src/trade/pages/{Trade,Positions,Access}.tsx` → `cockpit/src/pages/trading/`; `cockpit/src/trade/components/{Watchlist,OrderTicket,StrategyTicket,TradeBlotter,InstrumentSearchModal}.tsx` → `cockpit/src/components/trading/`; `cockpit/src/trade/api/{liveUpdates.tsx,liveUpdatesCore.ts,refresh.ts,submitOrder.ts}` → `cockpit/src/api/`
- Modify: `cockpit/src/routes.tsx`, tests importing moved modules (`liveUpdates.test.ts`, `refresh.test.ts`)

**Interfaces:**
- Consumes: `useSession()`, `TopBar`, merged `api`.
- Produces: `TradePage` (no `me` prop; reads `useSession()` and the granted portfolios it fetched before), `PositionsPage`, `AccessPage`.

- [ ] **Step 1:** Move files; fix imports; `npm test` → PASS (pure tests untouched apart from paths).
- [ ] **Step 2:** Port to shadcn: tickets use `Form` + zod schemas mirroring today's validation rules; Buy/Sell toggle uses `bg-buy`/`bg-sell`; instrument search is a `Dialog` containing `Command`; blotter and positions use `Table` with `.num` right-aligned numeric cells and `text-buy`/`text-sell` for side and P&L. Same columns, actions and confirmations as today.
- [ ] **Step 3:** Verify in dev: place and cancel an order (testnet or paper), strategy ticket, watchlist add/remove, positions refresh, live updates tick, Access page with `?reason=no_account`.
- [ ] **Step 4:** Commit `feat(ui): trading pages on shadcn`.

---

### Task 6: Admin Panel — People and Trading setup

**Files:**
- Move + port: `cockpit/src/pages/{Users,Groups,SignIn,Api,Portfolios,Accounts,Strategies,RiskLimits,Instruments}.tsx` → `cockpit/src/pages/admin/`
- Port in place: `cockpit/src/components/{CrudResource,InstrumentSelect,IbkrImport,Lifecycle,DeploymentPanel,UniverseStatus,apiTheme}.tsx`

**Interfaces:**
- Consumes: `AdminPanel` outlet, `toast`, merged `api`.
- Produces: `CrudResource` keeps its current props (it is the shared list/create/edit/delete frame); internally `Table` + `Dialog` + `Form` + `AlertDialog` for delete confirmation.

- [ ] **Step 1:** Port `CrudResource` first; every page using it follows.
- [ ] **Step 2:** Port the nine pages and their components; same fields, validation, confirmations and copy.
- [ ] **Step 3:** Verify: `npm test && npm run build`; in dev, as admin: create/edit/delete a group, approve a pending user and add a user, create/revoke an API key, edit a portfolio, account, risk limit, strategy (enable + FIX acceptor restart), import IBKR instruments.
- [ ] **Step 4:** Commit `feat(ui): admin people and trading setup on shadcn`.

---

### Task 7: Admin Panel — Connectivity and Monitoring, global indicators

**Files:**
- Move + port: `cockpit/src/pages/{BrokerConnections,DataFeeds,Blotter}.tsx` → `cockpit/src/pages/admin/`
- Port in place: `cockpit/src/components/{CredentialsPanel,OrderTimeline,TaskCenter,JobsIndicator,SetupChecklist}.tsx`

**Interfaces:**
- Consumes: `TopBar` (JobsIndicator and TaskCenter mount there for admins, matching where the cockpit showed them).

- [ ] **Step 1:** Port pages and components; same behaviour (credential save/test/reload, feed status, blotter filters and order timeline, background jobs).
- [ ] **Step 2:** Verify in dev: save Binance testnet credentials (adapter registers without restart), data-feed status, blotter filter by portfolio/user, open an order's timeline, a running job shows in TaskCenter.
- [ ] **Step 3:** Commit `feat(ui): admin connectivity and monitoring on shadcn`.

---

### Task 8: Remove Mantine and the second app; serve one shell

**Files:**
- Delete: `cockpit/trade.html`, `cockpit/src/trade/` (remaining), `cockpit/src/theme.ts`, `cockpit/src/palette.ts`, any `@mantine/*` imports
- Modify: `cockpit/package.json` (drop `@mantine/*`), `cockpit/vite.config.ts` (single input `index.html`), `cockpit/check-palette.sh` (only `src/styles/tokens.css` allowed), `cockpit/src/main.tsx` (drop `MantineProvider`)
- Modify: `src/cockpit.rs` (one router), `src/main.rs:1108` and `:1137-1149` (mount once, unconditionally; drop `trade_router()`), `src/main.rs:~1766` (log `UI: {host}/app/`), `src/auth_api.rs:636` + test `:837`, `sanitize_return_to` tests, `src/setup/status.rs` `ui_links` (+ its tests/callers), docs-site and scripts referencing `/cockpit/` or `/trade/`, `cockpit/README.md`

**Interfaces:**
- Produces: `cockpit::router()` serving `/` → 307 `/app/`, `/app` → 307 `/app/`, `/app/` and `/app/*path` → `respond("index.html", …)`, `/ui/*path` assets. `NO_ACCOUNT_URL = "/app/access?reason=no_account"`. `ui_links(base, bundled) -> Vec<(String,String)>` returns `[("UI", "{base}/app/")]` when bundled (the `oidc` parameter goes).

- [ ] **Step 1: Update Rust tests first** in `src/cockpit.rs`: bare `/` redirects to `/app/`; `/app` redirects to `/app/`; deep link `/app/admin/broker-connections` serves the shell; `/cockpit/` and `/trade/` are 404; missing asset with extension stays 404. In `auth_api.rs`: `assert_eq!(NO_ACCOUNT_URL, "/app/access?reason=no_account")` and the `sanitize_return_to` cases rewritten for `/app/…`. In `setup/status.rs`: `ui_links("http://h", true) == [("UI", "http://h/app/")]`, `ui_links("http://h", false)` empty.
- [ ] **Step 2:** `cargo test -p oms cockpit:: auth_api:: setup::status` (adjust to the crate name) → FAIL.
- [ ] **Step 3:** Implement the server changes; remove the frontend leftovers listed above.
- [ ] **Step 4:** Verify: `grep -rn "@mantine" cockpit/src` empty; `cd cockpit && npm test && npm run build && sh check-palette.sh && sh check-api-base.sh`; `cargo build && cargo test --workspace`; `grep -rn "/cockpit/\|/trade/" src docs-site scripts readme.md` shows nothing stale.
- [ ] **Step 5:** Commit `feat(ui)!: one app at /app/, Mantine removed`.

---

### Task 9: Click-through and polish

- [ ] **Step 1:** `cd cockpit && npm run build && cd .. && cargo build`, run the OMS against a throwaway DB, open `http://localhost:3001/`.
- [ ] **Step 2:** Walk the spec's "Done when" list: admin sign-up, user self sign-up → pending → approve, forced password change, MFA enroll and prompt, SSO sign-in if configured, place/cancel an order, positions, every Admin Panel page, ⌘K as admin and as user, `/app/admin/users` as user (NoAccess), reload on a deep link, session expiry → one redirect to login and back.
- [ ] **Step 3:** Fix anything found (each fix its own commit). Check density matches the tokens (row ≈ 26px, numbers monospace, right-aligned).
- [ ] **Step 4:** Ask the owner for a look-over before opening the PR.
