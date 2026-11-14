# Users, roles and SSO: design

> **Superseded in part (2026-11-13):** the owner's authentication redesign in
> `2026-11-13-auth-redesign-instructions.md` overrides this document where they
> conflict — Authentik is no longer a runtime dependency; local accounts
> (Argon2id + TOTP) and provider-neutral OIDC replace it. Roles, portfolio
> authorization, invites, strategies and audit below remain in force.


Date: 2026-11-13. Status: draft, awaiting owner approval.

## Why

The owner wants a PM and a few traders from outside to test the OMS. Setting up a person today
means creating a principal, a password or API key and portfolio grants on separate admin pages.
For the trader app, the owner did not know where to start. The cockpit gives no sense of what to
do next. Keycloak is heavier than the owner wants to run and administer.

Success: the owner invites a tester in one dialog, sends them a link, and the tester signs in with
Microsoft, Google, GitHub or an email + password and lands in the trader app. They see only their
portfolios and can do only what their role allows. A first-time admin is led there by a setup
checklist.

## Goals

- **Invite-only onboarding.** Users → Invite user: email, role, portfolios. A single dialog
  creates everything. The cockpit shows an invite link for the owner to copy and send (no email
  sending in v1).
- **Sign-in via one OIDC provider, Authentik.** Authentik brokers Microsoft, Google and GitHub
  sign-in, plus local email + password accounts enrolled through the invite link. Authentik
  replaces Keycloak.
- **Roles.** Three built-in roles: PM, Trader and Viewer, plus the existing admin console. They
  are stored as data (role → permissions) so custom roles can come later without a schema change.
- **Portfolio-scoped authorization.** Role permissions apply only within the portfolios a user is
  granted. The backend enforces every check, deny by default.
- **Setup checklist** on the cockpit home (option A from the mockup), pinned until done. Each
  step opens the page that completes it:
  1. Connect a venue.
  2. Create a portfolio.
  3. Turn on sign-in.
  4. Invite a user.
  5. First order appears in the blotter.
- **Audit**: invites, acceptances, role changes, grant changes and revocations are recorded with
  who did them.

- **Docs follow the model.** Every diagram and relationship picture in the docs site is redrawn
  for users / strategies / roles / invites / identities, with Authentik in place of Keycloak.
  Known places:
  - `docs-site/src/components/ArchitectureDiagram.astro`;
  - the table/relationship diagrams in `operations/database.md` and
    `operations/master-key-and-backups.md`.

  Prose that names principals or Keycloak is rewritten too:
  - `configuration/login.md` becomes the Authentik + invites page;
  - `strategies/colocated.md`, `strategies/fix.md` and `strategies/rest.md`.

  The builder greps for any further diagram (`principal`, `keycloak`, `grant`) before closing.
  The changelog keeps its history.

## Non-goals (v1)

- UI polish. The cockpit pages here only need to work: plain forms and tables in the existing
  components and theme. The owner will redo the UI later. No new visual design, animations or
  custom components.
- Multi-tenant or organizations. The deployment is one firm, run by the owner.
- Emailed magic links or codes, and invite emails. Both need an email account; they come later.
- Role editor UI. The schema supports custom roles; v1 ships only the built-in three.
- SCIM provisioning, direct Entra ID federation, and an entitlement export API. These are
  *future*, and the design must not block them (see "Enterprise path").
- An external authorization engine (OpenFGA, Cerbos). Database tables plus one permission checker
  in the backend are enough.

## Anti-goals (would make this a failure even if it works)

- Uninvited people get *any* access, including read-only. They see "Access not granted".
- Email is used as the lasting identity. Email is only used to claim the invite. After that, the
  login is bound to the provider's `(issuer, subject)`.
- Permissions enforced only by hiding cockpit controls.
- Roles in Authentik that must be kept in sync with the OMS. Authentik answers "who is this", and
  the OMS owns everything else.
- An existing API key, strategy principal or the admin console stops working.

## Current state (what exists)

- `principal` table: `principal_type` HUMAN/SERVICE/STRATEGY/DESK, status ACTIVE/DISABLED, and
  `external_subject` UNIQUE. The subject is matched alone, without the issuer.
- `principal_portfolio_grant`: per principal and portfolio, with `can_view`, `can_trade` and
  `can_allocate` flags.
- `risk_limits`: per **portfolio + instrument** (order and position quantity/notional caps and a
  trading state). There are no per-trader limits today.
- OIDC login (`src/oidc.rs`, `src/auth_api.rs`, `src/sessions.rs`): authorization code with PKCE,
  a session cookie, idle and absolute TTLs, and an optional `required_claim` gate.
  `resolve_or_provision` currently **auto-creates** a HUMAN principal with no grants on first
  login.
- API keys (`api_key` table) for machine access, used through HTTP Basic or Bearer.
- The admin console authenticates with a single admin password or token (`OMS_ADMIN_PASSWORD`).
- `deploy/` and `docker-compose` run Keycloak for local OIDC.

## Design

### Vocabulary: users, strategies, principals

- **Principal** stays as the internal "who did this" record. Every order, cancel and audit entry
  points at one. It is never shown in the cockpit, the trader app or the user docs.
- **User**: a person who signs in through Authentik. A user row sits on top of exactly one
  principal.
- **Strategy**: an automated trader, registered as its own entity (not a user), on top of
  exactly one principal. This matches common fund practice and the MiFID II idea of an algorithm
  inventory: each algorithm is registered, has a responsible person and a kill switch, and every
  order says which algorithm made it.
- The `SERVICE` and `DESK` principal types are removed. API keys belong to a user or a strategy
  and are labelled that way ("Jonas's key", "mm1's key").

### Strategies

- This builds on the existing `strategy` table (migration 0035), which already has its own
  principal, an `owner_principal_id`, and a portfolio it trades exclusively.
- **Responsible user:** `owner_principal_id` becomes the required *responsible user*. It must be
  an ACTIVE user, and it is shown on the strategy. Handing the strategy over is an explicit,
  audited action.
- **Portfolio:** a strategy trades the portfolio it owns. This is today's one-portfolio rule,
  kept in v1. More than one portfolio per strategy comes later.
- **Who may create or edit strategies:**
  - the admin (cockpit), always;
  - a PM, for portfolios where they hold `limits.manage`; the responsible user is the PM or a
    user granted on that portfolio.
  - New permission: `strategies.manage` (PM ✓).
- **Its powers** come from the strategy's own grants on its portfolio, as today. A strategy's
  orders are attributed to the strategy's principal. The order also records the responsible user
  at submit time, for audit.
- **Disabling the responsible user** does not stop the strategy silently. It flags it "needs a
  new responsible user" in the cockpit and the checklist. A strategy without an ACTIVE
  responsible user cannot be (re)started.
- **Limits and kill switch** stay as they are today (strategy state, portfolio limits).
- **Desks/teams:** later. Portfolios act as books for now.

### Identity mapping

- A new table `principal_identity` with columns `(issuer, subject, principal_id, linked_at,
  last_login_at)`, unique on `(issuer, subject)`. It replaces the single
  `principal.external_subject` lookup. The existing column is migrated into it, and then dropped.
- **First login:** the OMS verifies the ID token. If the identity is unknown, the OMS looks for a
  pending, unexpired invite whose token arrived with the login flow (the invite link carries it),
  and checks it against the token's **verified** email (`email_verified = true`). If both match,
  the OMS links the identity to the invited user, marks the invite accepted and creates the
  session. Otherwise the user sees "Access not granted" and nothing is created.
- **Email mismatch is refused** (owner's decision): the invite link is a bearer credential, not
  proof of identity. If the verified email differs from the invited one (case-insensitive compare),
  the login is denied with "This invite is for j…@fundco.com. Sign in with that address, or ask
  for a new invite." Nothing is linked and the invite stays pending.
- `email_verified` must be `true`. Authentik is configured so it says so only when it holds:
  - Google and Microsoft: as asserted by the provider;
  - GitHub: the account's primary verified email;
  - local accounts: those enrolled through the invite link, whose email Authentik fixes to the
    invited address.
  The setup doc spells out this mapping. A missing or false claim is refused, with the reason
  shown.
- **Later logins** look up `(issuer, subject)` only.
- **Linking a second provider** (for example GitHub after Microsoft) is an explicit action by a
  signed-in user, or a fresh invite from an admin. Two logins are never merged because their
  emails match.
- Auto-provisioning in `resolve_or_provision` is removed.

### Invites

- Table `invite`: `id`, `token_hash`, `email`, `role`, portfolio list, `created_by`,
  `created_at`, `expires_at` (default 7 days), `accepted_at`, `accepted_principal_id`,
  `revoked_at`.
- Creating an invite creates the principal right away with status `INVITED`, the role assignment
  and the grants, so the users list shows it as "Invited". Accepting it sets the status to
  `ACTIVE`.
- The link is `{public_base_url}/auth/invite/{token}`. It starts the OIDC login with the invite
  bound to the flow (in the flow cookie). The Authentik login page offers the
  Microsoft/Google/GitHub buttons and email + password enrollment.
- Inviting is allowed before sign-in is set up. The dialog then warns: "Invitees can't sign in
  until sign-in is turned on." *(Builder-check ruling.)*
- An `INVITED` principal cannot authenticate by any route: no session, and its API keys are
  refused. Revoking an invite sets the principal `DISABLED` and keeps the row and its audit trail.
  *(Ruling.)*
- The owner can re-copy, revoke or re-issue an invite. A revoked or expired invite shows a
  specific message ("This invite expired. Ask for a new one."), never a generic one.

### Roles and permissions

- Tables:
  - `role (code, name, description, built_in)`;
  - `role_permission (role_code, permission)`;
  - `principal_role (principal_id, role_code)`.
- One role per user in v1. The schema allows more.
- Permissions (strings, checked in one place):

  | Permission | PM | Trader | Viewer |
  |---|---|---|---|
  | `orders.view_own` | ✓ | ✓ | ✓ |
  | `orders.view_all` (in the portfolio) | ✓ | | ✓ |
  | `orders.create` | ✓ | ✓ | |
  | `orders.cancel_own` | ✓ | ✓ | |
  | `orders.cancel_any` | ✓ | | |
  | `positions.view` | ✓ | ✓ | ✓ |
  | `limits.view` | ✓ | ✓ | ✓ |
  | `limits.manage` | ✓ | | |

- **"Own"** means `order_state.principal_id = caller`. That column is already written on every
  submit. This changes behaviour for people: today anyone with `can_trade` on a portfolio can
  cancel any order in it. Afterwards, Traders cancel only their own orders, and PMs can cancel any.
  *(Ruling.)*
- A permission applies only inside portfolios the principal is granted. The check is
  `allowed(principal, permission, portfolio)`. The existing grant flags become: `can_view` =
  in scope at all; `can_trade` = may use `orders.create` / `cancel_*` there (so a trader can be
  "trade A, view B"). `can_allocate` keeps its meaning.
- The built-in role rows are seeded by migration. Existing HUMAN principals get the PM role,
  so someone can still manage everything after the upgrade.
- Strategy principals, and API keys of existing SERVICE principals (migrated to a user or
  strategy owner; see Migration), keep working through their existing grants. Strategies get a
  `strategy` role with exactly today's powers: `orders.view_all`, `orders.create`,
  `orders.cancel_any`, `positions.view` and `limits.view`. Nothing that works today breaks.
  *(Ruling.)*
- The admin console stays separate (admin password or token) in v1. PM, Trader and Viewer use
  only the trader app. The checker covers every route that authenticates by session or API key
  (`AuthContext`). *(Ruling.)*

### Limits

- "PM sets limits" means `limits.manage` on the portfolio's existing `risk_limits` rows
  (portfolio + instrument). The trader app shows them read-only to Traders and Viewers. The
  cockpit admin pages keep full access.
- New invitees get no special limits; the portfolio's limits apply.

### Enforcement

- Every trading route resolves `AuthContext`, then calls the checker. Order list endpoints filter
  by `view_own` / `view_all` in SQL, not after the fetch. Cancel checks own vs. any.
- Tests:
  - change a portfolio or order ID in each request to one outside the user's scope → 403/404;
  - a Viewer cannot create orders;
  - a Trader cannot cancel someone else's order;
  - a PM can;
  - an uninvited login creates no rows;
  - an expired invite is refused;
  - identities are never merged on matching email.

### Revocation

- Disabling a user (or removing a grant) takes effect on the next request: sessions are checked
  against principal status per request, and the user's sessions are deleted on disable. API keys
  of a disabled principal stop working (the existing behaviour, verified by test).

### Cockpit (functional only; the UI gets reworked later)

Plain forms and tables in the existing components. The mockup shows *what's there*, not how it
should look.

- **Users page:** a table with name/email, role, portfolios, sign-in method and status
  (Active / Invited until <date> / Disabled). Actions: Invite, Copy link, Revoke invite, Change
  role, Change portfolios, Disable.
- **Invite form:** email, role (select), portfolios (checkbox + trade/view each). On submit, it
  shows the link with a Copy button and the expiry date.
- **Strategies page:** the existing page, plus the responsible user (select), a handover action,
  and the "needs a new responsible user" flag.
- **Setup checklist** on the home page: the five steps above as a list with links. Each step is
  detected from real state and hidden once all are done.
- **Sign-in settings:** shows the configured issuer, whether discovery succeeds (and the reason
  if not), and the redirect URL to paste into Authentik.
- **Trader app:** plain pages for "Access not granted", "Invite expired or revoked", and "Invite
  is for a different email".

### Authentik deployment

- Authentik runs as Docker services next to the OMS:
  - test server: compose stack under `/opt/oms`, exposed via Tailscale Serve like the OMS;
  - locally: in the dev compose, replacing Keycloak.
- A setup doc page explains how to:
  - create the OIDC application;
  - add Microsoft, Google and GitHub sources (each needs an app registration at that provider);
  - enable invite-based enrollment for email + password.
- The OMS-side config stays the existing `[auth.oidc]` block plus `OMS_OIDC_CLIENT_SECRET`.
- `required_claim` stays available but is not needed, because invites gate access.

### Migration and cutover *(ruling; prerelease, so harsh cleanups are fine)*

- Existing `SERVICE` principals: each with API keys becomes a user (no login identity, API key
  only), or is deleted if unused. The admin chooses per row in a one-off migration step; the
  default is "user". `DESK` rows are dropped if unused, otherwise converted to a user.

- `principal.external_subject` is backfilled into `principal_identity` using the configured
  `[auth.oidc]` issuer, then dropped.
- Keycloak is removed from compose and from the test server in the same release, with no
  coexistence period. Existing OIDC users re-link through a fresh invite if their issuer changes
  (Keycloak → Authentik changes it).

### Enterprise path (not built now; the design must allow it)

- Corporate SSO (Entra ID, Okta): either an Authentik source, or point `[auth.oidc]` at the
  corporate issuer directly. Identities are keyed by `(issuer, subject)`, so both work.
- SCIM: an inbound SCIM endpoint creating principals and invites. It fits the identity layer
  without touching permissions.
- Entitlement export: "who can do what on which portfolio" is a single join over `principal`,
  `principal_role`, `role_permission` and `principal_portfolio_grant`, so an export or API is a
  read-only endpoint over it.

## Left to the builder

- Migration ordering and the backfill of `external_subject` into `principal_identity`.
- The exact checker API shape and where it lives.
- Route-by-route mapping of existing endpoints to permissions.
- How the setup checklist detects "sign-in on" (OIDC configured and discovery OK).
- Cockpit component structure, reusing the existing theme (`palette`, JetBrains Mono).
