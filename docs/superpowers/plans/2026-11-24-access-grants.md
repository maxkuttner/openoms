# Access Grants Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open WebUI-style access: default grants + group grants (switches), per-portfolio access lists of groups or people, enforced on the server and reflected in the cockpit.

**Architecture:** `access_grant` rows (default or group) resolve to a set of `GrantKey`s per person; `authz::allowed` requires both portfolio access and the mapped grant; person-level portfolio access joins the `portfolio_access` view. Cockpit Groups and Portfolios pages follow the approved canvas boards.

**Tech Stack:** Rust (axum, sqlx, Postgres), React + shadcn/ui cockpit (`node --test` via `npm test`), Astro docs.

**Spec:** `docs/superpowers/specs/2026-11-24-access-grants-design.md`

## Global Constraints

- Worktree `/Users/max/workarea/oms-cockpit-redesign`, branch `feat/cockpit-redesign`.
- DB tests: ALL of `POSTGRES_HOST=127.0.0.1 POSTGRES_PORT=55432 POSTGRES_USERNAME=postgres POSTGRES_PASSWORD=pw POSTGRES_DATABASE=ods OMS_RELOAD_TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55432/ods OPENSSL_ROOT_DIR=/opt/homebrew/opt/openssl@3 OPENSSL_DIR=/opt/homebrew/opt/openssl@3`, `-- --ignored --skip real_docker`. Never without all of them. CI runs every `--ignored` test.
- Commit in a separate Bash call, subject-only. No `git stash`.
- Cockpit: shadcn/ui only. Groups stay under **People**.
- Grant keys exactly: `orders`, `cancel_any`, `allocate`, `options`, `strategies`, `strategy_tokens`, `limits_view`, `limits_set`, `api_keys`. Labels exactly as the spec table.
- Admins skip grant checks but still need portfolio access to trade. Strategy tokens and the host token are not grant-checked.
- Denied by a missing grant → 403 `not granted: <key>`.
- Next migrations: `0075`, `0076`; `embeds_every_migration` 75 → 77.

## Review Focus

1. After migration an existing trader can still place orders and a PM can still cancel any / set limits (Task 1 `migration_preserves_current_access`).
2. A person in two groups gets the union; turning a switch off in one group doesn't remove it if the other or the default grants it (Task 1 `grants_are_a_union`).
3. Person-level access and group access on the same portfolio: highest level wins, removing the person entry falls back to the group level (Task 2 `person_and_group_access_highest_wins`).
4. Deleting a group removes its grants (cascade) (Task 1 `group_delete_drops_its_grants`).
5. Admin with no portfolio access still can't trade, but can create strategies/API keys with all grants off (Task 3 `admin_skips_grants_not_access`).

---

### Task 1: Grant model, table, resolution

**Files:** Create `db/migrations/ods/oms/0075_ACCESS_GRANTS.sql`, `src/access_grants.rs`; modify `src/main.rs` (mod), `src/setup/database/assets.rs` (count).

**Interfaces:**
- Produces: `pub enum GrantKey { Orders, CancelAny, Allocate, Options, Strategies, StrategyTokens, LimitsView, LimitsSet, ApiKeys }` with `as_str()`, `parse(&str) -> Option<Self>`, `label()`, `ALL`; `pub async fn grants_for(pool, principal: Uuid) -> sqlx::Result<BTreeSet<GrantKey>>` (admins → all); `pub async fn default_grants(conn) / group_grants(conn, group)`; `pub async fn set_default_grants(tx, ctx, &[GrantKey])`, `set_group_grants(tx, ctx, group, &[GrantKey])` (replace set; audit `grants.default_set` / `grants.group_set`).
- Migration: table per spec (`subject_type` 'default'|'group', `subject_id` NULL for default, FK to `user_group(id) ON DELETE CASCADE`, unique on `(subject_type, coalesce(subject_id, '00000000-0000-0000-0000-000000000000'), grant_key)` via unique index); seed default keys `orders, allocate, options, strategies, strategy_tokens, limits_view, api_keys`; seed `cancel_any, limits_set` for every group with a `manage` row in `user_group_portfolio`.

- [ ] **Step 1:** Failing tests: unit `keys_round_trip`; DB `grants_are_a_union`, `group_delete_drops_its_grants`, `admin_has_every_grant`, `migration_preserves_current_access` (run the seed against a fixture: manage group → has cancel_any/limits_set; plain user → orders, limits_view).
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test --bin oms access_grants::` (+ `--ignored` env) and `embeds_every_migration` → PASS.
- [ ] **Step 5:** Commit `feat(grants): default and group access grants`.

### Task 2: Person-level portfolio access

**Files:** Create `db/migrations/ods/oms/0076_PORTFOLIO_PERSON_ACCESS.sql`; create `src/portfolio_access_api.rs`; modify `src/main.rs`.

**Interfaces:**
- Migration: `portfolio_person_access(portfolio_id uuid FK portfolio, principal_id uuid FK principal, access text CHECK view|trade|manage, PK(portfolio_id, principal_id))`; `CREATE OR REPLACE VIEW portfolio_access` adding a person branch combined with group access per (principal, portfolio) using the highest level (keep strategies' direct-grant precedence and column list unchanged).
- Produces: `GET /admin/portfolios/:id/access -> Vec<AccessEntry>`, `PUT` same body (full replace of both group rows in `user_group_portfolio` for this portfolio and person rows); `AccessEntry { kind: "group"|"person", id: Uuid, name: String, access: "view"|"trade"|"manage" }`. Audit `portfolio.access_set`. Strategy groups' rows for their own portfolio can't be removed (422 `strategy group keeps its portfolio`).

- [ ] **Step 1:** Failing DB tests `person_and_group_access_highest_wins`, `put_access_replaces_both_kinds`, `strategy_group_row_is_kept`.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement. **Step 4:** module tests + `authz::` + `groups_api::` with `--ignored` env → PASS.
- [ ] **Step 5:** Commit `feat(access): person-level portfolio access`.

### Task 3: Enforcement and /auth/me

**Files:** Modify `src/authz.rs` (`allowed`, add `require_grant`), callers for non-portfolio grants: `src/reported/strategy.rs` (create → `strategies`; rotate/revoke/archive → `strategy_tokens`), `src/users_api.rs` (`create_own_key` → `api_keys`), `src/order_entry.rs` (option/combo instrument → `options`), allocation handler (`allocate`), `src/auth_api.rs` (`/auth/me` adds `grants: Vec<String>`).

**Interfaces:**
- `Permission::grant(self) -> Option<GrantKey>`: OrdersCreate/OrdersCancelOwn→Orders, OrdersCancelAny→CancelAny, LimitsView→LimitsView, LimitsManage→LimitsSet, others None. `allowed` = existing check AND (grant None OR principal has it OR principal is admin). Machine principals (strategy, host-cli) skip the grant part.
- `pub async fn require_grant(pool, ctx: &AuthContext, key: GrantKey) -> Result<(), ApiError>` → 403 `not granted: <key>`.
- Option detection: the instrument's asset/instrument class marks options or a combo (find the existing field in the instrument catalog; options live in `instrument_derivative`).

- [ ] **Step 1:** Failing DB tests: `orders_grant_off_blocks_submit`, `cancel_any_needs_grant`, `limits_set_needs_grant`, `strategies_grant_blocks_create`, `api_keys_grant_blocks_key_create`, `options_grant_blocks_option_order`, `admin_skips_grants_not_access`, `strategy_token_not_grant_checked`, `me_lists_grants`.
- [ ] **Step 2:** → FAIL. **Step 3:** Implement; update existing tests whose fixtures relied on implicit grants (seed defaults via the migration, which runs in `migrated_db`).
- [ ] **Step 4:** run `authz::`, `authz_route_tests`, `order_entry::`, `handlers::`, `reported::`, `users_api::`, `groups_api::` with `--ignored` env, and unit `cargo test --bin oms` → PASS.
- [ ] **Step 5:** Commit `feat(authz): enforce access grants`.

### Task 4: Grants admin API

**Files:** Modify `src/access_grants.rs` (handlers), `src/main.rs`.

**Interfaces:** `GET/PUT /admin/grants/default` and `GET/PUT /admin/groups/:id/grants` (body/response `{ grants: string[] }`, unknown key → 422 `unknown grant <key>`), `GET /admin/groups/:id/preview -> { grants: [{ key, label, granted, from: "default"|"group" }] }`, `GET /admin/grants/catalog -> [{ key, label, category }]` (categories Trading / Strategies / Risk / Account, order as spec). All under `admin_middleware`.

- [ ] **Step 1:** Failing tests `put_default_grants_round_trips`, `unknown_grant_is_422`, `preview_marks_default_vs_group`, `catalog_lists_all_keys_in_order`.
- [ ] **Step 2–4:** fail, implement, pass.
- [ ] **Step 5:** Commit `feat(grants): admin grants API`.

### Task 5: Cockpit Groups page

**Files:** Rewrite `cockpit/src/pages/admin/Groups.tsx` per board "Admin · Groups & grants"; create `cockpit/src/accessGrants.ts` + test.

**Interfaces:** `resolveGrants(defaults: string[], group: string[]): { key: string; on: boolean; inherited: boolean }[]` (inherited = in defaults); list with "Default grants" first, then groups (name + "N members · M portfolios"); editor tabs General / Users / Portfolios / Grants / Preview (Default grants shows only Grants); switches = shadcn `Switch`, inherited ones disabled with "from default" Badge; Portfolios tab edits this group's rows (reuse `PUT /admin/groups/:id/portfolios`); Save per tab.

- [ ] **Step 1:** failing `node --test` for `resolveGrants`. **Step 2–4:** fail, implement, `npm test`, `npx tsc -b`, `npm run build`.
- [ ] **Step 5:** Commit `feat(cockpit): groups and grants editor`.

### Task 6: Cockpit Portfolio editor and feature hiding

**Files:** Modify `cockpit/src/pages/admin/Portfolios.tsx` (table columns Portfolio / Access / Venues / Status; side editor with tabs Access / Venues / General); move venue checklist from `components/admin/PortfolioGrants.tsx` into the Venues tab; create `components/admin/PortfolioAccess.tsx`; use `/auth/me` `grants` to hide: Strategies create button (`strategies`), token actions (`strategy_tokens`), API key create (`api_keys`), cancel buttons on others' orders (`cancel_any`), limits pages/edit (`limits_view`/`limits_set`).

**Interfaces:** `hasGrant(me, key): boolean` (admins true) in `cockpit/src/accessGrants.ts` with tests; Access tab: add group or person (shadcn Command search), View/Trade/Manage ToggleGroup, remove; PUT `/admin/portfolios/:id/access`.

- [ ] **Step 1:** failing tests for `hasGrant`. **Step 2–4:** fail, implement, `npm test`, `tsc`, build.
- [ ] **Step 5:** Commit `feat(cockpit): portfolio access editor, hide ungranted features`.

### Task 7: Docs

**Files:** `docs-site/src/content/docs/configuration/login.md` (Groups section → "Groups and grants": default grants, group grants union, the grant table with labels, admins skip grants, portfolio access lists with people).

- [ ] **Step 1:** edit. **Step 2:** `npm run build`, `npm test` in docs-site. **Step 3:** Commit `docs: access grants`.
