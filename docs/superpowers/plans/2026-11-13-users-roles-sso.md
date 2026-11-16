# Users, Roles and SSO Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Invite-only users who sign in through Authentik (Microsoft, Google, GitHub, email + password), with PM / Trader / Viewer roles enforced per portfolio in the backend, strategies as their own entity with a responsible user, and a setup checklist in the cockpit.

**Architecture:** Principals stay as the internal actor record. New tables:
- `principal_identity`: `(issuer, subject)` → principal;
- `invite`;
- `role` / `role_permission` / `principal_role`;
- `audit_event`.

One checker in `src/authz.rs` answers `allowed(principal, permission, portfolio)`, and every `AuthContext` route calls it. The OIDC callback claims an invite instead of auto-provisioning. Authentik replaces Keycloak in compose.

**Tech Stack:** Rust (axum, sqlx, Postgres 16), React + Mantine cockpit, Astro docs site, Authentik (Docker).

**Spec:** `docs/superpowers/specs/2026-11-13-users-roles-sso-design.md`

## Global Constraints

- Uninvited sign-ins create no rows and get "Access not granted". No automatic read-only access.
- Email only claims an invite. Every later login resolves by `(issuer, subject)`. Two identities are never merged because their emails match.
- An invite claim needs `email_verified = true`, and the verified email must equal the invited one (case-insensitive). Otherwise the login is refused and the invite stays pending.
- Permissions are enforced in the backend, deny by default. Hiding UI controls is never the enforcement.
- "Own" order means `order_state.principal_id = caller`.
- Built-in roles PM, Trader and Viewer, plus a `strategy` role. One role per principal in v1, stored as data.
- Every API key, strategy and colo link that works today keeps working. Existing HUMAN principals become Trader. Strategy principals get `strategy`.
- The word "principal" never appears in cockpit UI text, trader-app text or docs-site prose.
- UI is functional only: plain forms and tables in existing components and theme. No new visual design.
- Migrations start at **0047**, because PR #28 (fees) owns 0046. If #28 is not merged when this branch lands, rebase and renumber.
- DB tests run against a throwaway Postgres only (see HANDOFF), never `oms-postgres` or the shared dev DB. With `--ignored`, always pass `--skip real_docker`.
- Commits use Conventional Commits, written via a message file and `git commit -F`. Stage files explicitly.

## Review Focus

1. **An ID from outside the caller's scope** (order, portfolio or strategy UUID swapped in the URL or body): it must return 403 (exists) or 404, never data. Pinned in Task 3: `idor_*` tests.
2. **A Trader cancelling another trader's order in the same portfolio:** must be 403. Today it succeeds. Pinned in Task 3.
3. **The same person signing in again through a second provider with the same email:** must not reach the first account, and must get "Access not granted" unless explicitly invited or linked. Pinned in Task 4: `second_provider_same_email_is_not_merged`.
4. **Disabling a user while their session is open:** the very next request must be 401, and their API keys must stop. Pinned in Task 5: `disable_kills_sessions_and_keys`.
5. **A strategy whose responsible user is disabled:** it must be refused at enable/start and flagged in the list. Pinned in Task 6: `enable_refused_without_active_responsible_user`.

---

### Task 1: Schema

**Files:**
- Create the following in `db/migrations/ods/oms/`:
  - `0047_CREATE_PRINCIPAL_IDENTITY.sql`
  - `0048_CREATE_ROLES.sql`
  - `0049_CREATE_INVITE.sql`
  - `0050_CREATE_AUDIT_EVENT.sql`
  - `0051_ALTER_PRINCIPAL_STATUS_AND_TYPE.sql`
  - `0052_ALTER_ORDER_STATE_ADD_RESPONSIBLE.sql`
- Modify: `src/setup/database/assets.rs:96` (oms migration count 46 → 52; or 47 → 53 after the #28 rebase).
- Test: `src/setup/database/assets.rs`, plus a new `src/authz_schema_tests.rs` (`#[ignore]` DB tests).

**Interfaces:**
- Produces:
  - `principal_identity(issuer TEXT, subject TEXT, principal_id UUID FK, linked_at, last_login_at)` with `UNIQUE(issuer, subject)`.
  - `role(code PK, name, description, built_in BOOL)`, `role_permission(role_code FK, permission TEXT, PK both)`, `principal_role(principal_id PK FK, role_code FK)`.
  - `invite(id UUID PK, token_hash TEXT UNIQUE, email TEXT, principal_id UUID FK, created_by TEXT, created_at, expires_at, accepted_at NULL, revoked_at NULL)`.
  - `audit_event(id, at, actor TEXT, action TEXT, target_principal_id UUID NULL, detail JSONB)`.
  - `principal.status` ∈ `ACTIVE | INVITED | DISABLED`; `principal_type` ∈ `HUMAN | STRATEGY`.
  - `order_state.responsible_principal_id UUID NULL`.

- [ ] **Step 1:** Write `#[ignore]` DB test `migrations_backfill_identities_and_roles`:
  - seed (before migrating 0047+) a HUMAN principal with `external_subject='sub-1'`, a SERVICE principal with an API key, a DESK principal without keys, and a STRATEGY principal;
  - run migrations with `OMS_OIDC_ISSUER_FOR_BACKFILL` set to `https://idp.test`;
  - assert:
    - `principal_identity` has `('https://idp.test','sub-1')`;
    - the `external_subject` column is gone;
    - the HUMAN principal has role `trader` and the STRATEGY principal has role `strategy`;
    - the SERVICE principal is now HUMAN with role `trader`;
    - the unused DESK principal is deleted.

  The spec's "admin chooses per row" step for SERVICE principals is the default conversion to a key-only user. An unused one is deleted; an admin can disable leftovers afterwards from Users.

  The backfill issuer comes from a session setting the migrator sets from `[auth.oidc].issuer`. If none is configured, rows with `external_subject` are kept as unlinked principals, and their owners re-link via invite.
- [ ] **Step 2:** Run it on a throwaway Postgres and confirm it fails (tables missing).
- [ ] **Step 3:** Write the migrations.
  - Seed the roles `pm`, `trader`, `viewer` and `strategy` with the permission matrix:
    - pm: `orders.view_own, orders.view_all, orders.create, orders.cancel_own, orders.cancel_any, positions.view, limits.view, limits.manage, strategies.manage`
    - trader: `orders.view_own, orders.create, orders.cancel_own, positions.view, limits.view`
    - viewer: `orders.view_own, orders.view_all, positions.view, limits.view`
    - strategy: `orders.view_all, orders.create, orders.cancel_any, positions.view, limits.view`
  - Add a CHECK that `strategy.owner_principal_id` is not a STRATEGY principal, via a trigger, because a CHECK cannot read another table.
- [ ] **Step 4:** Update the migration count test. Run `cargo test` (unit) and the ignored test. Both should PASS.
- [ ] **Step 5:** Commit `feat(db): identities, roles, invites and audit tables`.

### Task 2: Permission checker

**Files:**
- Create: `src/authz.rs`
- Modify: `src/main.rs` (add `mod authz`)
- Test: `src/authz.rs` (`#[cfg(test)]` unit tests, plus `#[ignore]` DB tests)

**Interfaces:**
- Consumes: the Task 1 tables.
- Produces:
  - `pub enum Permission { OrdersViewOwn, OrdersViewAll, OrdersCreate, OrdersCancelOwn, OrdersCancelAny, PositionsView, LimitsView, LimitsManage, StrategiesManage }` with `fn as_str(self) -> &'static str` matching the seeded strings.
  - `pub async fn allowed(pool: &PgPool, principal_id: Uuid, perm: Permission, portfolio_id: Uuid) -> Result<bool, sqlx::Error>`. True only if all hold:
    - the principal is ACTIVE;
    - its role has `perm`;
    - a grant exists on the portfolio with `can_view`;
    - for `OrdersCreate`, `OrdersCancelOwn` and `OrdersCancelAny`, the grant also has `can_trade`.
  - `pub async fn require(pool, principal_id, perm, portfolio_id) -> Result<(), ApiError>`: 403 when not allowed.
  - `pub async fn visible_portfolios(pool, principal_id, perm) -> Result<Vec<Uuid>, sqlx::Error>`.

- [ ] **Step 1:** Write DB tests:
  - `viewer_cannot_create`;
  - `trader_can_create_with_can_trade_only` (with a view-only grant: false);
  - `pm_can_manage_limits`;
  - `disabled_principal_is_never_allowed`;
  - `invited_principal_is_never_allowed`;
  - `no_grant_means_no_access`.
- [ ] **Step 2:** Run them and confirm they FAIL (module missing).
- [ ] **Step 3:** Implement as one SQL query per call (join `principal`, `principal_role`, `role_permission`, `principal_portfolio_grant`).
- [ ] **Step 4:** Run the tests: PASS.
- [ ] **Step 5:** Commit `feat(authz): role and portfolio permission checker`.

### Task 3: Enforce on trading routes

**Files:**
- Modify:
  - `src/handlers.rs`: `require_order_grant` (lines ~147–200) and its callers, `list_orders`, `get_order`, `list_portfolios`, `get_portfolio_positions`, `get_portfolio_venues`, `create_allocations` / `list_allocations`.
  - `src/order_entry.rs`: the grant check at ~703–728 and cancel at ~930.
  - `src/order_events.rs`.
  - `src/live_updates.rs`: filter streamed orders.
- Test: the existing DB test modules next to each handler.

**Interfaces:**
- Consumes: `authz::{allowed, require, visible_portfolios, Permission}`.
- Produces:
  - `require_order_grant(pool, principal_id, order_id, OrderAccess) -> Result<(), ApiError>` with `enum OrderAccess { View, Cancel }`.
    - View passes with `OrdersViewAll`, or with `OrdersViewOwn` when the order is the caller's.
    - Cancel passes with `OrdersCancelAny`, or with `OrdersCancelOwn` when the order is the caller's.
    - A missing order is 404; an unentitled one is 403 (unchanged).
  - Submit requires `OrdersCreate` on the target portfolio, and stamps `order_state.responsible_principal_id`:
    - for a strategy's principal: its `strategy.owner_principal_id`;
    - otherwise: the caller.

- [ ] **Step 1:** Write DB tests:
  - `trader_cannot_cancel_other_traders_order`: 403.
  - `pm_can_cancel_any_order_in_portfolio`.
  - `trader_list_orders_shows_only_own`.
  - `viewer_list_orders_shows_all_in_portfolio`.
  - `idor_get_order_outside_scope_is_403`.
  - `idor_positions_outside_scope_is_403`.
  - `idor_submit_to_ungranted_portfolio_is_403`.
  - `strategy_api_key_can_still_submit_and_cancel` (regression).
  - `submit_stamps_responsible_principal`.
  - `ws_updates_skip_orders_outside_scope`.
- [ ] **Step 2:** Run them and confirm they FAIL. `trader_cannot_cancel_other_traders_order` currently returns 200.
- [ ] **Step 3:** Implement. `list_orders` filters in SQL:
  - `os.portfolio_id = ANY(view_all portfolios)`, OR
  - `(os.portfolio_id = ANY(view_own portfolios) AND os.principal_id = caller)`.
- [ ] **Step 4:** Run the full `cargo test` and the ignored DB suite (`--skip real_docker`): PASS.
- [ ] **Step 5:** Commit `feat(authz): enforce roles on order, position and stream routes`.

### Task 4: Invites and identity linking

**Files:**
- Create: `src/invites.rs` (create / revoke / reissue / claim).
- Modify:
  - `src/auth_api.rs`: replace `resolve_or_provision`; add `GET /auth/invite/:token`; carry the invite token in `FlowState`.
  - `src/oidc.rs`: `VerifiedIdentity` gains `issuer: String` and `email_verified: bool`.
  - `src/main.rs`: routes.
- Test: `src/invites.rs` and `src/auth_api.rs` tests.

**Interfaces:**
- Consumes: the Task 1 tables.
- Produces:
  - `invites::create(pool, email: &str, role: &str, grants: &[(Uuid /*portfolio*/, bool /*can_trade*/)], created_by: &str) -> Result<NewInvite, InviteError>`
    - `NewInvite { invite_id, principal_id, token: String /* plaintext, returned once */, expires_at }`.
    - Creates the principal (HUMAN, status INVITED, code derived from the email's local part, made unique), its role, its grants (`can_view = true`, `can_trade` per the argument) and the audit event `invite.created`.
    - Expiry is 7 days.
  - `invites::revoke(pool, invite_id, actor)`: sets `revoked_at`, sets the principal DISABLED, audits `invite.revoked`.
  - `invites::reissue(pool, invite_id, actor) -> NewInvite`: a new token and expiry for the same principal.
  - **Linking a second provider (spec):**
    - `invites::create_link(pool, principal_id, actor) -> NewInvite` issues an invite for an ACTIVE principal, with the email taken from the user.
    - Claiming it adds a second `principal_identity` row; the principal is already ACTIVE, so it stays ACTIVE.
    - Exposed as `POST /admin/users/:id/link-invite` (Task 5) and tested by `link_invite_adds_second_identity`.
  - `pub enum ClaimOutcome { Linked { principal_id, code }, Known { principal_id, code }, NoInvite, Expired, Revoked, EmailMismatch { invited_hint: String }, EmailUnverified }`
  - `invites::resolve_login(pool, identity: &VerifiedIdentity, invite_token: Option<&str>) -> Result<ClaimOutcome, sqlx::Error>`:
    - a known `(issuer, subject)` with an ACTIVE principal → `Known`, and `last_login_at` is updated;
    - otherwise it claims the invite if one is present and valid, in one transaction:
      - insert `principal_identity`;
      - set the principal ACTIVE and `accepted_at`;
      - audit `invite.accepted`.
  - `invited_hint` masks the address: the first letter of the local part, `…`, then the domain (`j…@fundco.com`).
  - Callback outcomes:
    - `Linked` / `Known`: session, then redirect.
    - Every other outcome: redirect to `/trade/access?reason=no_invite|expired|revoked|email_mismatch|email_unverified` (plus `&hint=` for a mismatch).
  - `GET /auth/invite/:token`: starts the login flow with the token stored in the flow cookie, and nothing else.

- [ ] **Step 1:** Write tests:
  - `uninvited_login_creates_no_rows` (row counts of `principal` and `principal_identity` unchanged);
  - `invite_claim_links_identity_and_activates`;
  - `email_mismatch_is_refused_and_invite_stays_pending`;
  - `email_compare_is_case_insensitive`;
  - `unverified_email_is_refused`;
  - `expired_invite_is_refused`;
  - `revoked_invite_is_refused`;
  - `second_provider_same_email_is_not_merged` (the second issuer with the same email and no invite gives `NoInvite`);
  - `invite_token_is_single_use` (a second claim gives `NoInvite`);
  - `token_stored_hashed_only` (the plaintext is not in the DB).
- [ ] **Step 2:** Run the tests: FAIL.
- [ ] **Step 3:** Implement. Hash the token like session tokens (`sessions::hash_session_token`). Delete `resolve_or_provision` and its tests, because auto-provisioning is gone.
- [ ] **Step 4:** Run the tests: PASS. Run the full suite.
- [ ] **Step 5:** Commit `feat(auth): invite-only sign-in bound to issuer and subject`.

### Task 5: Users admin API and revocation

**Files:**
- Create: `src/users_api.rs`.
- Modify:
  - `src/admin.rs`: remove the `/admin/principals*` handlers (lines ~29–275 and the grants/keys handlers around ~2000–2180). Move key creation under users and strategies.
  - `src/sessions.rs`: session lookup requires `status = 'ACTIVE'`, which it already does for HUMAN; keep that and add a test.
  - `src/auth.rs`: `verify_key` requires an ACTIVE principal.
  - `src/main.rs`: routes.
- Test: `src/users_api.rs`.

**Interfaces:**
- Consumes: `invites::{create, revoke, reissue}`, `authz`.
- Produces these admin routes (admin token, as other `/admin/*` routes):
  - `GET /admin/users` → `[UserRow { principal_id, code, display_name, email, role, portfolios: [{portfolio_id, code, can_trade}], sign_in: Option<String> /* issuer host of last login */, status, invite: Option<{invite_id, expires_at}> }]` (HUMAN only).
  - `POST /admin/users/invite { email, role, portfolios: [{portfolio_id, can_trade}] }` → `{ invite_id, link, expires_at }`, where `link = {public_base_url}/auth/invite/{token}`.
  - `POST /admin/users/invites/:id/revoke`
  - `POST /admin/users/invites/:id/reissue` → as invite.
  - `PUT /admin/users/:id/role { role }`
  - `PUT /admin/users/:id/portfolios [{portfolio_id, can_trade}]`
  - `POST /admin/users/:id/disable`: deletes the user's sessions and audits.
  - `POST /admin/users/:id/enable`
  - `GET|POST /admin/users/:id/keys` and `DELETE /admin/users/:id/keys/:key_id`.

  Each mutation writes an `audit_event`.

- [ ] **Step 1:** Write tests:
  - `disable_kills_sessions_and_keys` (a session request returns 401 and an API key request returns 401 after disable);
  - `invited_user_api_key_is_refused`;
  - `invite_endpoint_returns_link_with_public_base_url`;
  - `role_change_is_audited`;
  - `list_users_excludes_strategies`.
- [ ] **Step 2:** Run the tests: FAIL.
- [ ] **Step 3:** Implement. Delete the `/admin/principals` routes and update their callers in `cockpit/src` (Task 8 replaces the page).
- [ ] **Step 4:** Run the tests: PASS. Run the full suite.
- [ ] **Step 5:** Commit `feat(admin): users API replaces principals`.

### Task 6: Strategies with a responsible user

**Files:**
- Modify:
  - `src/strategies/admin.rs`: `create_strategy` ~268, `update_strategy` ~329, `enable_strategy` ~557, `list_strategies` ~224, and keys.
  - `src/main.rs`.
- Create: `src/strategies/trader_api.rs` (PM-facing).
- Test: `src/strategies/` test modules.

**Interfaces:**
- Consumes: `authz::require(.., Permission::StrategiesManage, portfolio)`.
- Produces:
  - `StrategyRow` gains `responsible: { principal_id, code, display_name, status }` and `needs_responsible: bool` (true when the responsible user is not ACTIVE).
  - Create/update validation: `owner_principal_id` must be an ACTIVE HUMAN. Otherwise return 422 "Pick an active user as responsible".
  - `POST /admin/strategies/:code/handover { principal_id }`: validated the same way, audits `strategy.handover`.
  - `enable_strategy` and colo start refuse with 409 "Strategy has no active responsible user" when `needs_responsible`.
  - Trader-app routes (`auth_middleware`):
    - `GET /strategies`: the strategies on portfolios where the caller has `StrategiesManage`.
    - `POST /strategies`: same body as admin create. Requires `StrategiesManage` on the portfolio. The responsible user must be the caller or a user granted on that portfolio.
    - `POST /strategies/:code/handover`.

- [ ] **Step 1:** Write tests:
  - `create_refuses_inactive_or_strategy_responsible`;
  - `handover_is_audited`;
  - `enable_refused_without_active_responsible_user`;
  - `pm_can_create_strategy_on_managed_portfolio`;
  - `trader_cannot_create_strategy` (403);
  - `pm_cannot_create_on_unmanaged_portfolio` (403).
- [ ] **Step 2:** Run the tests: FAIL.
- [ ] **Step 3:** Implement. Reuse the admin create logic through a shared `fn create_strategy_inner(pool, req, actor) -> Result<StrategyRow, StrategyError>`.
- [ ] **Step 4:** Run the tests: PASS. Run the full suite plus the colo tests (`cargo test -p colo-runtime`).
- [ ] **Step 5:** Commit `feat(strategies): responsible user, handover and PM creation`.

### Task 7: PM limits in the trader API

**Files:**
- Create: `src/limits_api.rs`.
- Modify: `src/main.rs`.
- Test: `src/limits_api.rs`.

**Interfaces:**
- Consumes: `authz`, the existing `risk_limits` table and the existing admin limit validation (reuse the function behind `cockpit/src/pages/RiskLimits.tsx`'s endpoint).
- Produces:
  - `GET /portfolios/:id/limits` (`LimitsView`) → the existing risk-limit row shape.
  - `PUT /portfolios/:id/limits/:instrument_id` (`LimitsManage`): same body and validation as admin. Audits `limits.changed` with before and after values.

- [ ] **Step 1:** Write tests:
  - `trader_can_view_limits`;
  - `trader_cannot_change_limits` (403);
  - `pm_change_is_audited_with_before_after`;
  - `idor_limits_outside_scope_is_403`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS, then the full suite.
- [ ] **Step 5:** Commit `feat(limits): PM-managed limits in the trader API`.

### Task 8: Cockpit and trader app (functional)

**Files:**
- Delete: `cockpit/src/pages/Principals.tsx`.
- Create: `cockpit/src/pages/Users.tsx`, `cockpit/src/components/SetupChecklist.tsx`, `cockpit/src/pages/SignIn.tsx`, `cockpit/src/trade/pages/Access.tsx`.
- Modify:
  - `cockpit/src/App.tsx` (nav: "Users" replaces "Principals"; checklist on home);
  - `cockpit/src/pages/Strategies.tsx` (responsible select, handover, flag);
  - `cockpit/src/trade/App.tsx` (route `/access`).
- Backend: `GET /admin/setup-status` and `GET /admin/auth/status` in `src/users_api.rs`.
- Test: `cockpit/src/*.test.ts` for pure helpers, plus Rust tests for the two status endpoints.

**Interfaces:**
- Consumes: the Task 5 and 6 routes.
- Produces:
  - `GET /admin/setup-status` → `{ venue_connected: bool, portfolio_exists: bool, sign_in_ok: bool, user_invited: bool, first_order: bool }`. Each value is computed from DB state and the OIDC discovery state.
  - `GET /admin/auth/status` → `{ configured: bool, issuer: Option<String>, discovery_ok: bool, error: Option<String>, redirect_uri: Option<String> }`.

- [ ] **Step 1:** Write Rust tests:
  - `setup_status_reflects_db_state` (empty DB: all false; after seeding: each flips);
  - `auth_status_reports_unconfigured`.

  Write the cockpit test `checklistSteps` (a pure function mapping status to an ordered step list with links). The step labels are exactly:
  1. "Connect a venue"
  2. "Create a portfolio"
  3. "Turn on sign-in"
  4. "Invite a user"
  5. "First order shows in the blotter"

  The checklist is hidden when all are true.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement with existing Mantine components and the theme, as plain tables and forms.
  - Invite form fields: email, role select (PM / Trader / Viewer), portfolios (checkbox plus a trade/view select).
  - After submitting, show the link with a copy button and "Send this link to {email}. It expires on {date}."
  - If `sign_in_ok` is false, show the warning "Invitees can't sign in until sign-in is turned on."
  - Access page texts, by `reason`:
    - `no_invite`: "Access not granted. Ask the OMS admin for an invite."
    - `expired`: "This invite expired. Ask for a new one."
    - `revoked`: "This invite was revoked."
    - `email_mismatch`: "This invite is for {hint}. Sign in with that address, or ask for a new invite."
    - `email_unverified`: "Your sign-in provider didn't confirm your email address. Use another sign-in method or ask for a new invite."
  - No UI text says "principal".
- [ ] **Step 4:** Run `cd cockpit && npm test && npm run build`, then `cargo test`: PASS. Run the app manually:
  - invite → copy link → open it in a private window;
  - with sign-in unconfigured, check that the warning shows.
- [ ] **Step 5:** Commit `feat(cockpit): users, invites, setup checklist and sign-in status`.

### Task 9: Authentik replaces Keycloak

**Files:**
- Modify: `docker-compose.yml`, `deploy/docker-compose.yml`, `scripts/dev-trade.sh`, `scripts/README.md`.
- Create: `deploy/authentik/README.md` (only if the deploy dir keeps per-service notes; otherwise the docs page in Task 10 covers it).

**Interfaces:**
- Produces:
  - compose services `authentik-server`, `authentik-worker`, `authentik-postgres` and `authentik-redis`, with pinned image tags (current stable at build time, recorded in the commit);
  - the local Authentik UI on `127.0.0.1:9000`.
  - The OMS `[auth.oidc]` example points at `http://localhost:9000/application/o/oms/`.
  - Keycloak services and volumes are removed.

- [ ] **Step 1:** Remove Keycloak and add Authentik. `scripts/dev-trade.sh` prints the steps to create the OIDC provider and application named `oms`.
- [ ] **Step 2:** Verify:
  - `docker compose config` is valid;
  - `docker compose up -d authentik-server authentik-worker` comes up healthy on 9000;
  - discovery succeeds: `curl -s localhost:9000/application/o/oms/.well-known/openid-configuration | jq .issuer`.

  Use project-scoped container names, and never touch `oms-postgres`.
- [ ] **Step 3:** Manual end to end:
  - invite yourself;
  - open the link;
  - sign in with a local Authentik account enrolled with the invited email;
  - land in the trader app;
  - sign in with a different email: you see the mismatch page.
- [ ] **Step 4:** Commit `chore(deploy): Authentik replaces Keycloak`.

### Task 10: Docs and diagrams

**Files:**
- Modify:
  - `docs-site/src/components/ArchitectureDiagram.astro`;
  - `docs-site/src/content/docs/configuration/login.md` (it becomes "Sign-in and users": Authentik setup; the Microsoft, Google and GitHub sources; invite enrollment for email + password; the `email_verified` mapping per source from the spec; roles and the permission matrix; inviting, revoking and disabling);
  - `operations/database.md` and `operations/master-key-and-backups.md` (table/relationship diagrams: users, strategies, identities, roles, invites, audit_event);
  - `strategies/colocated.md`, `strategies/fix.md`, `strategies/rest.md` (strategy plus responsible user; keys belong to the strategy).
- Modify: `docs/openapi.json` (regenerate with `cargo run -- openapi > docs/openapi.json`).

- [ ] **Step 1:** Update every page listed.
  - Then run `grep -rniE "principal|keycloak" docs-site/src --exclude=changelog.md`. Every remaining hit must be either an internal API field name in a code sample, or gone. Rewrite any other hit.
  - Check every `grant` hit for the old meaning.
- [ ] **Step 2:** Regenerate the OpenAPI file. Run `cd docs-site && npm run check`: PASS.
- [ ] **Step 3:** Commit `docs: users, roles, strategies and Authentik sign-in`.

### Task 11: Final verification

- [ ] **Step 1:** Run `cargo test --workspace` and `cargo clippy --workspace --all-targets` (no new warnings in touched files).
- [ ] **Step 2:** Run the ignored DB suite on a throwaway `postgres:16` (CI-style env from HANDOFF) with `--skip real_docker`, 3 times: all green.
- [ ] **Step 3:** Run `cockpit npm test && npm run build` and `docs-site npm run check`.
- [ ] **Step 4:** Repeat the manual end-to-end from Task 9 Step 3, plus:
  - PM sees every order in the portfolio, while a Trader sees only their own;
  - a Trader's cancel of the PM's order is refused;
  - the existing strategy API key still trades.
