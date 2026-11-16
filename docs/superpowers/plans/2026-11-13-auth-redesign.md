# Auth Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the shared admin password and the mandatory Authentik login with named OpenOMS accounts. These accounts use local credentials (Argon2id password plus TOTP) and/or one optional, provider-neutral OIDC issuer, behind one session system for the cockpit and the trader app. PR #30's roles, portfolio checks, invites, strategies and audit stay.

**Architecture:**
- Every human, admin included, is a HUMAN principal. A principal signs in either:
  - with a `local_credential` (Argon2id + TOTP), or
  - with a linked `principal_identity` from the configured OIDC issuer.
- Both paths mint the same `user_session` row, with `auth_method` and `mfa_verified_at`.
- `/admin/*` and the trading routes use the same `authenticate()`. Admin is a global capability (`principal_admin`), separate from the one role per principal.
- Order actions and admin mutations require MFA:
  - local sessions must have passed TOTP;
  - OIDC sessions follow the configured policy.
- The legacy shared password survives only as a DB-flagged transition path until the first named admin signs in with MFA.

**Tech Stack:**
- Rust: axum 0.7, sqlx 0.8, openidconnect 4.
- New crates: `argon2` 0.5 and `totp-rs` 5 (versions verified at install).
- React 18 + Mantine 7 cockpit.
- Postgres 16.

**Spec:** `docs/superpowers/specs/2026-11-13-auth-redesign-instructions.md` (binding; cited §N). It supersedes parts of `docs/superpowers/specs/2026-11-13-users-roles-sso-design.md`.

## Owner decisions (2026-11-13)

- **MFA scope:** a local account must have passed TOTP in the current session before any order action (submit, cancel, allocate) or any admin mutation. Read-only access needs no MFA. Paper-only trading is not guaranteed by code (delta §7), so the gate does not key off `PAPER`/`LIVE`.
- **Entra / unverified-email SSO:** the admin-approved `(issuer, subject)` binding (§6) is in scope for this round.
- **Legacy admin:** the shared password keeps working until a named admin has signed in with MFA. After that, the DB flag turns it off. Host CLI recovery is the fallback.
- **Owner's account:** it gets the admin capability **and** a PM role. Admin alone never grants trading.

## Global Constraints

- Keep PR #30's role/permission tables, `src/authz.rs`, portfolio grants, invites, strategies, API keys and audit. Extend them; don't fork them (§0, §2, §8).
- Migrations 0047–0053 stay as they are. All schema changes go in new migrations from **0054** on, and the count test in `src/setup/database/assets.rs` is updated.
- `config.rs` uses `deny_unknown_fields`. Keep `[server].admin_password` and `[auth.oidc]` parseable: deprecated, with a warning, never an error.
- No homegrown crypto. Use Argon2id via `argon2` with params at or above m=19456 KiB, t=2, p=1, run inside `spawn_blocking` behind a semaphore. Use TOTP via `totp-rs` (RFC 6238, SHA-1, 6 digits, 30 s). Encrypt TOTP secrets with the existing master key (`src/secrets.rs`).
- Never log or return any of these: passwords, TOTP secrets, recovery codes, invite tokens, session tokens, OIDC codes or tokens.
- No public registration and no auto-provisioning. Invites are the only way in, apart from the first admin set up via bootstrap (§3, §7).
- `(issuer, subject)` is the only key for a returning OIDC identity. Email is never a merge key (§2).
- No default credentials. Remove the loopback `openoms-dev` default and the `OMS_ADMIN_AUTH_ENABLED=false` bypass. Refuse to boot when `OMS_OIDC_CLIENT_SECRET=oms-dev-secret` and the bind is not loopback (§9).
- No mandatory Docker services for auth. Authentik stays only as an opt-in `--profile oidc` dev fixture (§9).
- The word "principal" never appears in user-visible UI or docs prose.
- DB tests run only against a throwaway Postgres. Always run `--ignored` with `--skip real_docker`, and never touch `oms-postgres`, `oms-keycloak` or 100.80.233.3.
- Commits use Conventional Commits, written via `git commit -F`, with files staged explicitly.

## Review Focus

1. **Upgrade lockout:** an existing install with only the shared password must reach the cockpit after upgrade and be able to create a named admin. Pinned in Task 5: `legacy_admin_can_bootstrap_named_admin` and `legacy_admin_refused_after_named_admin_mfa_login`.
2. **Password-only local session places an order:** must be 403 with an "MFA required" reason. API keys are unaffected. Pinned in Task 6: `local_session_without_mfa_cannot_submit`.
3. **Invite token forwarded to someone else:** that person can't claim it with a different verified OIDC email, and an unverified OIDC login only creates a pending binding with no access. Pinned in Task 8: `pending_binding_grants_no_access_until_approved`.
4. **Disabled user with an open live-update socket:** the socket closes within one keep-alive tick, at most 15 s, and no further events are sent. Pinned in Task 7: `ws_closes_when_principal_disabled`.
5. **Last admin removes their own admin capability, or disables themselves:** refused (409). Pinned in Task 5: `last_admin_cannot_be_removed`.

---

### Task 1: Schema for local auth, sessions, admin and bindings

**Files:**
- Create the migrations in `db/migrations/ods/oms/`:
  - `0054_ALTER_USER_SESSION_ADD_AUTH_METHOD.sql`
  - `0055_CREATE_PRINCIPAL_ADMIN.sql`
  - `0056_CREATE_LOCAL_CREDENTIAL.sql`
  - `0057_CREATE_PENDING_IDENTITY_BINDING.sql`
  - `0058_ALTER_INVITE_ADD_METHOD.sql`
  - `0059_CREATE_SYSTEM_SETTING.sql`
- Modify: `src/setup/database/assets.rs` (count 54 → 60).
- Test: `src/authz_schema_tests.rs`.

**Interfaces (Produces):**
- `user_session` gains:
  - `auth_method TEXT NOT NULL DEFAULT 'oidc' CHECK (auth_method IN ('oidc','local','legacy_admin'))`;
  - `mfa_verified_at TIMESTAMPTZ NULL`.
- `principal_admin (principal_id UUID PK REFERENCES principal(id), granted_by TEXT NOT NULL, granted_at TIMESTAMPTZ NOT NULL DEFAULT now())`. A trigger rejects STRATEGY principals.
- `local_credential`:
  - `principal_id UUID PK FK`, `password_hash TEXT NOT NULL`, `password_changed_at`;
  - `totp_secret_enc BYTEA NULL`, `totp_enrolled_at NULL`, `totp_last_step BIGINT NULL`;
  - `failed_attempts INT NOT NULL DEFAULT 0`, `locked_until TIMESTAMPTZ NULL`.
- `recovery_code (principal_id FK, code_hash TEXT, used_at NULL, PRIMARY KEY (principal_id, code_hash))`.
- `pending_identity_binding`:
  - `id UUID PK`, `invite_id UUID FK`, `issuer`, `subject`, `untrusted_email`, `display_name`, `metadata JSONB`;
  - `created_at`, `expires_at`, `decided_at NULL`, `decided_by NULL`, `decision TEXT NULL CHECK (decision IN ('approved','rejected'))`;
  - `UNIQUE (issuer, subject) WHERE decision IS NULL`.
- `invite.method TEXT NOT NULL DEFAULT 'any' CHECK (method IN ('any','local','oidc'))`.
- `system_setting (key TEXT PK, value JSONB NOT NULL, updated_at)`, seeded with `('legacy_admin_enabled', 'true')`.

- [ ] **Step 1:** Write the `#[ignore]` DB test `auth_redesign_schema`. On a fresh migrated DB, assert:
  - each new table and column exists;
  - `legacy_admin_enabled = true`;
  - inserting a STRATEGY principal into `principal_admin` fails;
  - a second open `pending_identity_binding` for the same `(issuer, subject)` fails;
  - `user_session.auth_method` defaults to `oidc`.
- [ ] **Step 2:** Run it on a throwaway DB and confirm it FAILS.
- [ ] **Step 3:** Write the migrations and update the count.
- [ ] **Step 4:** Run the test and the full ignored suite: PASS.
- [ ] **Step 5:** Commit `feat(db): schema for local accounts, named admins and pending bindings`.

### Task 2: `[auth]` config and always-on session routing

**Files:**
- Modify: `src/config.rs` (~133-185, 376-399), `src/main.rs` (~1025-1062, 1374-1389), `src/sessions.rs`.
- Test: `src/config.rs`, `src/main.rs` (`the_full_router_builds`).

**Interfaces:**
- Produces config sections:
  - `[auth] public_base_url: Option<String>, policy: "local_and_sso" | "sso_only"` (default `local_and_sso`);
  - `[auth.local] enabled: bool` (default true);
  - `[auth.session] idle_ttl_minutes` (30), `absolute_ttl_hours` (12).
  - `[auth.oidc]` keeps its fields. Its `public_base_url` is still read as a fallback for `[auth].public_base_url`, with a deprecation warning.
  - `[auth.oidc]` gains `trust_email_verified: bool` (default true) and `require_amr: Vec<String>` (default empty).
- Produces `SessionConfig`, built from `[auth]`/`[auth.session]` whether OIDC is configured or not.
- Route mounting:
  - `/trade/*`, `/auth/me`, `/auth/logout`, `/auth/logout-all` and the local-login routes are mounted whenever `[auth.local].enabled` or OIDC is configured.
  - `/auth/oidc/*` is mounted only when discovery succeeded.

- [ ] **Step 1:** Write the tests:
  - `auth_section_parses_with_defaults`.
  - `legacy_oidc_public_base_url_still_used`.
  - `old_oms_toml_with_server_admin_password_still_parses`.
  - Extend `the_full_router_builds`: it asserts the router builds for (local only), (OIDC only) and (both).
- [ ] **Step 2:** Run them: FAIL.
- [ ] **Step 3:** Implement. Rename the OIDC routes to `/auth/oidc/login` and `/auth/oidc/callback`. Keep `/auth/callback` as an alias, because the registered redirect URIs depend on it.
- [ ] **Step 4:** Run `cargo test --workspace`: PASS.
- [ ] **Step 5:** Commit `refactor(auth): session and login routes no longer depend on OIDC config`.

### Task 3: Local credential primitives

**Files:**
- Create: `src/local_auth.rs`.
- Modify: `Cargo.toml` (`argon2`, `totp-rs` with `otpauth` + `gen_secret`), `src/main.rs` (`mod`).
- Test: `src/local_auth.rs`.

**Interfaces (Produces):**
- `pub async fn hash_password(pw: &str) -> Result<String, LocalAuthError>` (Argon2id PHC string, spawn_blocking, semaphore of 4).
- `pub async fn verify_password(hash: &str, pw: &str) -> bool`, constant-time via the library.
- `pub fn check_password_policy(pw: &str) -> Result<(), &'static str>`:
  - at least 15 characters;
  - at most 128 characters;
  - not in a built-in list of the 10k most common passwords (`include_str!` of a vendored list);
  - no composition rules.
- `pub struct TotpEnrollment { pub secret_enc: Vec<u8>, pub otpauth_uri: String }`.
- `pub fn new_totp(issuer: &str, account: &str, master: &MasterKey) -> TotpEnrollment`.
- `pub fn verify_totp(secret_enc: &[u8], master: &MasterKey, code: &str, last_step: Option<i64>, now: i64) -> Result<i64 /*step*/, TotpError>`:
  - accepts ±1 step;
  - rejects any step ≤ `last_step` (replay).
- `pub fn new_recovery_codes() -> (Vec<String> /*shown once*/, Vec<String> /*sha256 hashes*/)`: 10 codes of 128-bit random, base32, grouped.
- `pub fn verify_recovery_code(code: &str, hash: &str) -> bool`.

- [ ] **Step 1:** Write the tests:
  - `argon2_hash_verifies_and_rejects_wrong`;
  - `password_policy_length_and_common`;
  - `totp_accepts_current_and_adjacent_step`;
  - `totp_rejects_replayed_step`;
  - `totp_secret_is_encrypted_at_rest` (the ciphertext doesn't contain the base32 secret);
  - `recovery_codes_unique_and_hashed`.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(auth): Argon2id passwords, TOTP and recovery codes`.

### Task 4: Local login, enrolment, sessions and rate limiting

**Files:**
- Create: `src/local_auth_api.rs`.
- Modify: `src/sessions.rs`, `src/invites.rs`, `src/auth_api.rs`, `src/main.rs`.
- Test: `src/local_auth_api.rs`.

**Interfaces:**
- Consumes `local_auth::*` and `invites::*`.
- Produces these routes, all JSON. Each mutation is Origin-checked, and every response sets `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.
  - `GET /auth/invite/:token/preview` → `{ valid, email_hint, role, portfolios:[code], methods:["local","oidc"], expires_at }`.
    - It reveals the role and portfolios only for a valid, pending token.
    - An invalid token returns `{valid:false}`, with the same shape and timing class.
  - `POST /auth/local/enroll { token, password }`:
    - only for invites with `method` in (`any`, `local`), and only if the principal has no `local_credential` yet;
    - sets the password and claims the invite in one tx: identity `issuer='urn:openoms:local'`, `subject=principal_id`, principal ACTIVE, `accepted_at` set, audit `invite.accepted.local`;
    - creates a session with `auth_method='local'`, `mfa_verified_at=NULL`;
    - returns `{ next: "totp_enroll" }`.
  - `POST /auth/local/login { email, password }`:
    - on success, creates a session with `mfa_verified_at=NULL` and returns `{ next: "totp" | "totp_enroll" }`;
    - on failure, the error is generic: `{error:"invalid_credentials"}`.
  - `POST /auth/local/totp { code }`: on the current session, verifies the code (or a recovery code) and sets `mfa_verified_at`. **Rotates** the session: new token, old row revoked.
  - `POST /auth/local/totp/enroll` → `{ otpauth_uri }`. Requires the current session and no TOTP yet. Only a pending secret is stored at this point.
  - `POST /auth/local/totp/confirm { code }`: activates TOTP, sets `mfa_verified_at`, and returns `{ recovery_codes }` once.
  - `POST /auth/logout-all`: revokes every session of the caller.
- Rate limiting, in memory, keyed by lowercased email and by IP:
  - 5 failures per 15 min per account;
  - 20 per 15 min per IP.
  - When exceeded, return a generic 429, and still spend an Argon2 verify against a dummy hash so timing stays uniform.
  - Separately, `local_credential.locked_until` is set after 10 consecutive failures, for 15 min.
- `sessions::create_session(pool, principal_id, auth_method, mfa_verified: bool)`, plus `rotate_session` and `revoke_all_for_principal`. This re-adds the function removed in T5.

- [ ] **Step 1:** Write the tests:
  - `enroll_sets_password_claims_invite_and_needs_totp`.
  - `enroll_refused_for_oidc_only_invite`.
  - `enroll_refused_when_credential_exists`.
  - `login_wrong_password_generic_error`.
  - `login_unknown_email_same_error`.
  - `totp_success_rotates_session`.
  - `recovery_code_single_use`.
  - `rate_limit_blocks_after_5_account_failures`.
  - `lockout_after_10_failures`.
  - `preview_reveals_scope_only_for_valid_token`.
  - `logout_all_revokes_every_session`.
  - `no_secrets_in_responses_or_logs`, using a log-capture helper that greps for the password, secret and token.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS, then run the full suite.
- [ ] **Step 5:** Commit `feat(auth): local sign-in, invite enrolment and TOTP`.

### Task 5: Named admins, legacy cutover, bootstrap and recovery

**Files:**
- Modify:
  - `src/auth.rs` (`admin_middleware` ~196-212);
  - `src/main.rs` (~276, 337-357, 871-1005, 1251-1288; CLI `Command`);
  - `src/users_api.rs` (`ACTOR` → ctx; admin grant/revoke);
  - `src/setup/init/{mod,prompts,rules}.rs`.
- Create: `src/admin_bootstrap.rs`.
- Test: `src/admin_bootstrap.rs`, `src/users_api.rs`.

**Interfaces:**
- Produces `admin_middleware`, which resolves the request through `authenticate()` (session cookie) and requires a `principal_admin` row for an ACTIVE principal.
  - **Legacy path:** only while `system_setting.legacy_admin_enabled = true`, a `Bearer <shared password>` is also accepted. It yields an `AuthContext` for a synthetic actor `legacy-admin`. That actor may only:
    - `GET` any admin route;
    - `POST /admin/users/bootstrap-admin`.
  - Every other mutation by `legacy-admin` returns 403 `"create a named admin first"`.
  - `OMS_ADMIN_AUTH_ENABLED` and the `openoms-dev` default are removed. The boot warning names the replacement.
- `POST /admin/users/bootstrap-admin { email, display_name }`:
  - creates a HUMAN principal (INVITED, with `principal_admin`) and an invite with `method='local'`;
  - returns `{ link }`;
  - audit `admin.bootstrap`.
- When a session with `principal_admin`, `auth_method='local'` and a set `mfa_verified_at` exists for the first time, `legacy_admin_enabled` is set to false (audit `admin.legacy_disabled`). An OIDC admin session counts as well when the SSO MFA policy (Task 6) is met.
- Admin capability routes:
  - `PUT /admin/users/:id/admin {admin: bool}`;
  - refused if it would leave zero ACTIVE admins (409).
  - `disable_user` gets the same last-admin check.
- Audit actor = `AuthContext.principal_code` for every admin handler, so the hardcoded `const ACTOR = "admin"` goes.
- CLI commands, all audited with actor `host-cli`:
  - `oms auth bootstrap-admin --email E [--name N]`:
    - works only when no ACTIVE admin exists, or when `--force-recovery` is passed;
    - prints a one-time enrolment link and never writes a password.
  - `oms auth recover-admin --email E`: resets that admin's local credential and TOTP, revokes their sessions, and prints a new enrolment link.
- `oms init`:
  - asks for the admin's email and name, applies migrations, runs `bootstrap-admin` and prints the link;
  - no longer writes `[server].admin_password`.

- [ ] **Step 1:** Write the tests:
  - `legacy_admin_can_bootstrap_named_admin`.
  - `legacy_admin_cannot_mutate_otherwise`.
  - `legacy_admin_refused_after_named_admin_mfa_login`.
  - `admin_routes_need_admin_capability` (a PM session gets 403).
  - `admin_does_not_imply_trading` (an admin-only session can't submit an order).
  - `last_admin_cannot_be_removed`.
  - `last_admin_cannot_disable_self`.
  - `audit_actor_is_named_admin`.
  - `cli_bootstrap_refused_when_admin_exists`.
  - `cli_recover_revokes_sessions`.
  - `no_default_admin_password_on_loopback`.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS, then the full suite. Also run `cargo run -- auth bootstrap-admin --help` to confirm the CLI is wired up.
- [ ] **Step 5:** Commit `feat(auth): named admins replace the shared admin password`.

### Task 6: MFA gate

**Files:**
- Modify: `src/auth.rs` (`AuthContext` gains `auth_method: AuthMethod`, `mfa: bool`), `src/authz.rs`, `src/order_entry.rs`, `src/handlers.rs` (allocations), `src/strategies/trader_api.rs`, `src/limits_api.rs`, `src/auth.rs` (`admin_middleware`), `src/oidc.rs` (`amr`).
- Test: `src/authz_route_tests.rs`.

**Interfaces:**
- Produces `pub fn require_mfa(ctx: &AuthContext) -> Result<(), ApiError>`, which returns 403 `{"error":"mfa_required"}` unless one of these holds:
  - `auth_method == ApiKey`, since machine identities are out of scope (§8);
  - `auth_method == Local && mfa`;
  - `auth_method == Oidc` and either `require_amr` is empty (the IdP policy is trusted, and this is documented) or the token's `amr` contains every listed value. The `amr` is stored at login as `mfa_verified_at`.
- It is called on:
  - order submit, cancel and allocate;
  - strategy create and handover (trader API);
  - limits PUT;
  - every non-GET `/admin/*` route.

- [ ] **Step 1:** Write the tests:
  - `local_session_without_mfa_cannot_submit`.
  - `local_session_with_mfa_can_submit`.
  - `local_session_without_mfa_can_read_orders`.
  - `api_key_unaffected_by_mfa_gate`.
  - `oidc_session_ok_when_no_amr_required`.
  - `oidc_session_refused_when_amr_required_and_missing`.
  - `admin_mutation_needs_mfa`.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS, then the full suite.
- [ ] **Step 5:** Commit `feat(authz): require MFA for order and admin actions`.

### Task 7: Live-update sockets follow revocation

**Files:**
- Modify: `src/live_updates.rs` (~33, 154-159, 220-235), `src/sessions.rs`, `src/users_api.rs`.
- Test: `src/live_updates.rs`.

**Interfaces (Produces):**
- On every keep-alive tick (15 s), each socket re-checks its session: not revoked, not expired, and principal ACTIVE. If the check fails, it sends a close frame `{"type":"auth_revoked"}` and closes.
- On disable, role change, portfolio change or logout-all, `users_api` and `sessions` publish an `AuthChanged(principal_id)` on the existing broadcast. Sockets of that principal re-check immediately and refresh their scope.

- [ ] **Step 1:** Write the tests:
  - `ws_closes_when_principal_disabled`;
  - `ws_closes_on_logout_all`;
  - `ws_scope_refreshes_immediately_on_grant_change` (no 15 s wait; use paused tokio time).
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(stream): close sockets on revocation and refresh scope on grant changes`.

### Task 8: Provider-neutral OIDC with admin-approved binding

**Files:**
- Modify: `src/auth_api.rs` (~114-175, 214-249, 383-419), `src/invites.rs` (~280-411), `src/users_api.rs` (`link_invite`), `src/main.rs` (dev-secret refusal).
- Test: `src/invites.rs`, `src/auth_api.rs`.

**Interfaces:**
- **Server-side login flow.** The flow cookie carries only a random `flow_id`. A new in-memory map keyed by the SHA-256 of `flow_id` holds `{pkce_verifier, nonce, state, invite_token_hash, return_to, expires}`, with a TTL of 5 min. No raw invite token sits in a cookie.
- **`resolve_login` keeps its verified-email path.** On `EmailUnverified`, or on the `trust_email_verified=false` policy, with a valid pending invite:
  - insert a `pending_identity_binding`: issuer, subject, untrusted email, display name, `tid`/`hd` metadata, expiring with the invite;
  - return `ClaimOutcome::PendingApproval`;
  - redirect to `/trade/access?reason=pending_approval`.
  - The invite stays pending, and the binding has no access.
- Admin routes:
  - `GET /admin/users/bindings` lists the open bindings.
  - `POST /admin/users/bindings/:id/approve` binds atomically in one tx:
    - insert `principal_identity`;
    - set the principal ACTIVE and `accepted_at`;
    - set `decision`;
    - audit `binding.approved`.
  - `POST /admin/users/bindings/:id/reject`.
  - `POST /admin/users/:id/identities { issuer, subject }` is an explicit pre-binding, refused if that `(issuer, subject)` is already linked.
- **`link-invite` is tightened.** Claiming a link invite for an ACTIVE principal now needs one of:
  - a current session of that same principal, matched at callback;
  - admin approval via a pending binding.

  The mailbox token alone is no longer enough.
- **Dev secret refused.** Boot refuses `OMS_OIDC_CLIENT_SECRET=oms-dev-secret` when the bind is not loopback.

- [ ] **Step 1:** Write the tests:
  - `unverified_email_creates_pending_binding_not_session`.
  - `pending_binding_grants_no_access_until_approved`.
  - `approve_binding_links_and_activates_atomically`.
  - `reject_binding_keeps_invite_pending`.
  - `second_open_binding_for_same_identity_refused`.
  - `trust_email_verified_false_forces_approval`.
  - `link_invite_requires_owner_session`.
  - `prebind_identity_then_login_resolves`.
  - `flow_cookie_has_no_invite_token`.
  - `boot_refuses_dev_secret_off_loopback`.
  - Keep every existing invite and claim test green.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS, then the full suite.
- [ ] **Step 5:** Commit `feat(auth): admin-approved SSO binding and provider-neutral OIDC`.

### Task 9: Authentik becomes an opt-in fixture

**Files:**
- Modify: `deploy/docker-compose.yml` (remove the Authentik services and the `oms-dev-secret` default), `deploy/deploy.sh` (drop the `OMS_AUTHENTIK_*` requirement), `docker-compose.yml` (keep the `profiles: ["oidc"]` fixture and mark it dev-only), `docker/README.md`, `scripts/dev-trade.sh`, `scripts/README.md`.

- [ ] **Step 1:** Make the changes. The deploy stack runs only the OMS (plus whatever it ran before #30). Document how to point `[auth.oidc]` at any issuer.
- [ ] **Step 2:** Run `docker compose -f deploy/docker-compose.yml config -q` with a stub env file: valid, and no Authentik service left. Run `docker compose --profile oidc config -q`: still valid.
- [ ] **Step 3:** Commit `chore(deploy): Authentik is an opt-in dev fixture, not part of the deploy`.

### Task 10: Cockpit and trader app sign-in UI (functional only)

**Files:**
- Create: `cockpit/src/login/` (one login page used by both bundles), `cockpit/src/login/Enroll.tsx`, `Totp.tsx`, `InvitePreview.tsx`, `cockpit/src/login.ts` (pure helpers), `cockpit/src/login.test.ts`.
- Modify:
  - `cockpit/src/api/client.ts`: cookie session (`credentials: "same-origin"`), no `localStorage` token; a 401 redirects to `/login?return_to=…`; a 403 `mfa_required` opens the TOTP step.
  - `cockpit/src/pages/Login.tsx`: legacy password form shown only while `GET /auth/methods` reports `legacy_admin: true`.
  - `cockpit/src/pages/Users.tsx`: admin toggle, "Bootstrap named admin" when on legacy, pending-bindings list with approve/reject, link invite.
  - `cockpit/src/pages/SignIn.tsx`: generic SSO diagnostics.
  - `cockpit/src/trade/App.tsx`: the new `pending_approval` access reason.
- Backend: `GET /auth/methods` → `{ local: bool, sso: bool, sso_label: "Sign in with SSO", legacy_admin: bool }`.

**Interfaces:**
- Consumes the Task 4–8 routes.
- Copy, used verbatim:
  - Login: title "Sign in"; buttons "Sign in with SSO" (only when `sso`) and "Sign in" (email + password form, only when `local`).
  - Invite preview: "You're invited as {Role} on {portfolios}." Then the enabled methods: "Set a password" and/or "Sign in with SSO".
  - Enrol: "Choose a password (at least 15 characters)."
  - TOTP enrol: "Scan this code with an authenticator app, then enter the 6-digit code." Recovery codes: "Save these recovery codes now. Each works once."
  - TOTP: "Enter the 6-digit code from your authenticator app." Plus a link "Use a recovery code".
  - Access reason `pending_approval`: "Your sign-in is waiting for an administrator to approve it."
  - MFA prompt on 403: "Confirm with your authenticator code to continue."
- Render the QR client-side from `otpauth_uri` with a small vendored QR encoder (no new server dependency). If none is available in node_modules, show the URI and a copyable secret.

- [ ] **Step 1:** Write the tests in `cockpit/src/login.test.ts`:
  - `visibleMethods` (only enabled methods);
  - `nextStepFromLoginResponse` (totp / totp_enroll / done);
  - `accessMessage('pending_approval')`.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cd cockpit && npm test && npm run build`: PASS. Then grep `cockpit/src` for user-visible "principal": none.
- [ ] **Step 5:** Commit `feat(cockpit): one sign-in page, invite preview, password and TOTP enrolment`.

### Task 11: Docs and diagrams

**Files:**
- Modify:
  - `docs-site/src/content/docs/configuration/login.md`, rewritten as "Sign-in and users". It covers:
    - the local default;
    - first admin via `oms init` / `oms auth bootstrap-admin`;
    - inviting without SSO;
    - TOTP and recovery codes;
    - adding corporate OIDC (Entra / Google / Okta / Keycloak / Authentik), including `trust_email_verified` and approval of pending sign-ins;
    - `sso_only`;
    - admin recovery from the host;
    - upgrading from the shared password.
  - `docs-site/src/components/ArchitectureDiagram.astro`: Authentik is no longer required; the external IdP is optional.
  - `docs-site/.../operations/database.md` (new tables; migration count 60), `master-key-and-backups.md` (the TOTP secrets depend on the master key).
  - `README.md` (login section).
  - Also mark these specs as superseded where relevant: `docs/superpowers/specs/2026-11-13-users-roles-sso-design.md` §Authentik.
- [ ] **Step 1:** Write the pages above.
- [ ] **Step 2:** Grep `docs-site/src` (excluding the changelog) for `principal|oms-dev-secret|openoms-dev`. Remaining hits must be in code samples only.
- [ ] **Step 3:** `cd docs-site && npm run check`: PASS.
- [ ] **Step 4:** Commit `docs: local accounts, MFA and optional SSO`.

### Task 12: Verification

- [ ] **Step 1:** `cargo test --workspace` and `cargo clippy --workspace --all-targets` (no new warnings on branch lines).
- [ ] **Step 2:** The ignored DB suite three times, each on a fresh postgres:16, with `--skip real_docker`.
- [ ] **Step 3:** `cockpit npm test && npm run build`; `docs-site npm run check`.
- [ ] **Step 4:** Manual browser E2E (controller plus owner), over HTTPS through a local reverse proxy (Caddy on loopback):
  - (a) fresh install: `oms init`, admin enrolment link, password + TOTP, then the cockpit;
  - (b) upgrade path: the legacy password, bootstrapping a named admin, then the legacy password refused;
  - (c) local Trader invite: preview, password, TOTP, the trader app, then an order;
  - (d) an OIDC invite via the Authentik fixture with a verified email;
  - (e) an OIDC invite with `trust_email_verified=false`: pending approval, admin approves, then access;
  - (f) disabling the user closes their socket;
  - (g) checking cookie attributes behind the proxy.
