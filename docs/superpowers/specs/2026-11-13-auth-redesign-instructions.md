# OpenOMS authentication redesign — implementation instructions for an LLM coding agent

**Status:** Proposed implementation specification, 2026-11-13  
**Target:** [`maxkuttner/openoms`](https://github.com/maxkuttner/openoms), specifically the open [`feature/users-roles-sso` PR #30](https://github.com/maxkuttner/openoms/pull/30)  
**Priority:** Protect trading authorization and existing identities; reduce user-setup friction without building a general-purpose identity provider.

> **Agent mandate:** Implement a *small, self-hostable application's authentication layer*, not a replacement for Authentik or Keycloak. OpenOMS must own users, roles, portfolio grants, strategy ownership, and audit. It must accept local credentials or identity assertions from an optional external OIDC identity provider. Work incrementally; do not discard the considerable RBAC and invitation work already present in PR #30.
>
> **Do not execute changes based on file paths or schemas in this document without first checking the actual checkout.** I reviewed the public repository, PR description, branch design/implementation-plan documents, and the supplied sign-in walkthrough—not a full local checkout of PR #30. Verify code, migrations, API behavior, and any in-progress edits before implementing.

## 0. Architectural decision — read this first

**Choose: local accounts for self-hosted simplicity + generic external OIDC for organizations. Do not require, bundle, or operate Authentik as part of a normal OpenOMS installation.**

- **One OpenOMS user/principal model:** all interactive humans, including administrators, have named internal accounts. A human may have a local credential or explicitly linked external identities. The trading, cockpit, and API authorization layers resolve to the same internal principal ID.
- **One backend authorization implementation:** retain the PR's roles, role permissions, portfolio grants, `src/authz.rs`, disabled status, order ownership, limits, strategy ownership, API-key semantics, and audit events. The identity provider **never** decides who may trade a portfolio.
- **First-run experience:** `oms init` creates/bootstrap-enrolls the **first named administrator**, using a secure local method. `oms` starts without Authentik/Keycloak, SMTP, or internet access. A local administrator can create invitations in the cockpit.
- **Local human authentication:** use well-maintained Rust libraries for **password hashing (Argon2id)** and **TOTP MFA**, with recovery codes and administrative reset/re-enrollment. Local sign-in is password + TOTP for privileged users and for accounts permitted to place/cancel live orders. Do not invent cryptographic algorithms. A non-MFA local account must never quietly acquire live-trading permissions simply because its role grants them.
- **Optional generic OIDC:** support a correctly configured OIDC provider (Entra ID, Google, Okta, Keycloak, Authentik, etc.). The initial implementation can support one configured issuer; expose a neutral **“Sign in with SSO”** choice. Multiple independently configured issuers or branded social buttons are a separate small extension, not a reason to install an identity broker.
- **GitHub login:** standard GitHub OAuth login is **not equivalent to a generic OIDC provider**. Do not claim direct GitHub login works via generic OIDC. Defer native GitHub OAuth; users can use GitHub through an external broker such as Authentik if they need it.
- **Invite-only by default:** no public self-registration and no auto-provisioning on first SSO login. A new user is admitted only through a pending invitation, with evidence of identity appropriate to that method. Some enterprise IdPs (notably Microsoft Entra ID) do **not** reliably emit `email_verified`; support an explicit **administrator approval of a captured, validated `(issuer, subject)`** rather than silently treating their mutable `email` claim as verified.
- **Email OTP/magic link:** **not the recommended default**, despite the earlier “Direct” mockup. It introduces SMTP plus a lower-assurance login method. NIST SP 800-63B-4 explicitly excludes email as an out-of-band authenticator; email verification/recovery are distinct. If a future *sandbox-only* email-code mode is requested, specify it separately, label its limits, and do not use it for privileged or live-trading sessions.
- **Do not conflate “OIDC configured” with “MFA enforced.”** An OIDC provider's administrator must enforce an appropriate sign-in policy; OpenOMS can optionally require reliable `acr`/`amr` claims when supported, but may not assume an unspecified SSO login was multifactor.

### Reason for changing course from the current design

The supplied **“Current sign-in setup”** shows an admin inviting through OpenOMS, then manually creating a user in a second Authentik console, setting a password and an `email_verified` attribute, and sending credentials. This is not acceptable product onboarding. The current local Authentik setup runs server, worker, and its own PostgreSQL, and contains a published development OIDC client secret. The side-by-side mockup calls the alternative “Direct,” but a direct experience does *not* require writing email OTP login. Prefer a direct **OpenOMS login page with optional local login and a generic SSO button**, while preserving support for optional Authentik as an external OIDC provider.

### Explicit non-goals for this work

No SCIM provisioning, SAML, organization/multitenancy, arbitrary third-party auth plugins, GitHub OAuth, email OTP, broad role-editor redesign, external permission service, or replacement of the role/portfolio authorization model. Do not redesign unrelated order routing, FIX connectivity, or the trader UI. Do not add mandatory Docker containers to support authentication.

## 1. Investigate the real checkout before changing code

1. Run `git status --short`, inspect branch, and read the PR #30 commits versus `main`. Do not overwrite uncommitted work. Check the migrations already in `db/` and any migration conflicts (PR #30 notes that migration 0046 belongs to PR #28). Inspect the actual migration registry/count in `src/setup/database/assets.rs` before editing.
2. Read the existing source, not just documentation: OIDC routes/callback, session tables, current shared cockpit admin-password middleware, `src/authz.rs`, `/admin/users*` routes, invitation service, API-key authentication, websocket/SSE authorization, bootstrap/`oms init`, deploy compose, and config loading. Search the entire repository for `authentik`, `keycloak`, `OMS_OIDC_`, `email_verified`, `/admin/principals`, `principal_identity`, `invite`, and `principal_role`.
3. Read the existing design and implementation plan in the branch:
   - `docs/superpowers/specs/2026-11-13-users-roles-sso-design.md`
   - `docs/superpowers/plans/2026-11-13-users-roles-sso.md`
4. Produce a short implementation delta: **reuse unchanged**, **modify**, **remove/default-off**, **new components**, **migration risks**, and **current tests**. Prefer extending the existing code rather than creating parallel auth systems.
5. Identify whether an actual real-order transport can be used in local/test mode. Never assume “demo” means orders cannot reach a live venue. If a safe paper/simulated-only mode is not guaranteed by code, do not use that label as a security boundary.
6. Check current Rust, Axum, sqlx, React/Mantine versions and choose maintained, audited libraries. Do not write homegrown OAuth, Argon2, TOTP, token, or cookie cryptography.

**First deliverable:** a concise delta table and test baseline in the PR/implementation notes, then begin implementation. Only pause for owner input if an irreversible or high-risk migration cannot be resolved safely from the repository.

## 2. Canonical data/identity design

Preserve the existing PR's principal identity and authorization tables where possible. Names below are conceptual; adapt to real schema.

```text
principal (internal immutable actor ID; HUMAN | STRATEGY | KEY_ONLY as supported)
  id, display_name, email, status, ...

principal_identity
  id?, principal_id, provider_type, issuer, subject, created_at
  UNIQUE(issuer, subject)

local_credential
  principal_id UNIQUE, password_hash, password_changed_at,
  totp_secret_encrypted?, totp_enrolled_at?, recovery_code_hashes?, ...

invite
  existing PR fields: target principal/invitee email, role and portfolio grants,
  hashed one-time token, expires_at, consumed_at, revoked_at, issued_by, ...

session
  opaque session ID hash, principal_id, created_at, last_seen_at,
  expires_at, auth_method, mfa_verified_at?, revoked_at, ...

role / role_permission / principal_role / portfolio grant / audit_event
  REUSE PR #30 TABLES AND SEMANTICS
```

**Important invariants:**

- `(issuer, subject)` is the **stable identifier** for OIDC. Never identify a returning federated user solely by email, username, or display name.
- Local identity is linked to a principal by its credential's unique principal ID. Use a documented internal issuer namespace if preserving `(issuer, subject)` for *all* methods; never allow local and external issuers to collide.
- An email address is a contact property and a one-time invitation matching signal, **not a permanent global identity key**. Do not silently merge principals with matching emails. A verified provider email can change; the linked `(issuer, sub)` must continue to identify the same account, subject to policy.
- Invite consumption and principal/identity creation must be transactional, unique-constrained, and race-safe. Revoked, expired, already claimed, or superseded invitations cannot be accepted again.
- Existing HUMAN, STRATEGY, API key, key-only, existing OIDC identity and responsibility records must survive migrations. Preserve the PR's rule that disabling a user invalidates sessions/API keys and that strategies are distinct principals with accountable owners.
- Treat credentials as separate from principal/role data. Do not copy role or grant claims out of a user-controlled ID token. Server DB remains authoritative and is checked at request time.
- Maintain a unique, global internal admin identity with clear semantics. Admin is a **global application capability**, different from portfolio-scoped PM. Admin status must not accidentally allow trading without an explicit, auditable permission rule.

## 3. First-run, bootstrap, and administrator migration

1. `oms init` should create (or securely enroll) one **named admin** rather than establish an indefinitely shared website password. Keep the initial setup usable offline and without an IdP or mail server.
2. Choose one implementation route based on the real existing setup code:
   - **Preferred:** existing `oms init` securely collects admin username/email and a strong initial password, creates a HUMAN principal and local credential with global Admin permission, and requires TOTP enrollment before privileged remote use.
   - **Fallback:** secure, local, one-time CLI bootstrap (`oms auth bootstrap-admin`) restricted to an operator with host/database access, usable only when no active administrator exists. Do not expose a publicly accessible first-user-claims-admin endpoint.
3. If users upgrade from the single shared admin password, provide a safe, documented, one-time migration/transition mechanism. Do not assign existing traders global admin, and do not silently give the administrator PM/trading powers. Never drop the old login until the new admin can be confirmed to sign in; after cutover, disable the shared admin login.
4. Provide a securely controlled **break-glass recovery path** from the host CLI with auditable events and deliberate operator access. It must not be an always-available default credential or internet-accessible bypass; enterprise SSO-only policy and recovery must be documented together.
5. Ensure the setup checklist and existing cockpit experience remain functional throughout.

**Acceptance:** a clean local install requires only the OMS and its existing database; an admin can sign in without starting Authentik, Keycloak, or SMTP; upgraded installs do not lose administrative control.

## 4. One user-facing sign-in and session model

- Both `/cockpit/` and `/trade/` authenticate through a **single backend session mechanism** and the same principal. No shared cockpit password, bypass middleware, parallel SSO session cookie, or admin-only credentials sent to the front-end.
- The UI may have different entry routes, but one login page (e.g., `/login`) must present **only enabled authentication methods**. Unconfigured buttons never appear. Show the invitation's intended role/portfolios **before** redirecting to SSO, without leaking this information to non-holders of a valid token.
- A human with global Admin can open the cockpit; a PM/Trader/Viewer may open the trader UI only as allowed. Navigation and API guards must independently enforce the permissions. A valid session is not permission to access every API.
- Create a new session after successful authentication; invalidate pre-auth session material. Use strong opaque random IDs, preferably store only hashes in the DB, rotate/regenerate IDs on authentication or privilege changes. `Secure` (on HTTPS), `HttpOnly`, host-only and appropriate `SameSite` cookies. Never put access tokens/session tokens in `localStorage`, URL query strings or application logs.
- Configure idle and absolute TTLs (reuse sensible existing defaults: 30 min idle / 12 h absolute where applicable), logout, logout-all, server-side revocation, and last-admin protection. Server reads an ACTIVE principal and up-to-date grants for *every* privileged request.
- Implement CSRF defenses for cookie-authenticated mutations (`Origin`/`Referer` checks with safe fallback or CSRF token); `SameSite` alone is insufficient. No mutation through GET endpoints. Validate `Origin` and session/permission on WebSocket upgrades; recheck/revoke streaming access promptly when entitlements change.
- Do not log OIDC authorization codes, bearer tokens, cookies, passwords, recovery codes, invite tokens, TOTP secrets, or sensitive order data. Use structured audit records for accepted/rejected sign-ins and security changes, but minimize personal data.
- HTTPS is required except explicit loopback development. Respect trusted reverse proxy/public base URL settings and forbid spoofed forwarded headers.

## 5. Local accounts: simple, but security-conscious

**Why local accounts instead of default email codes:** They require no SMTP/IdP, allow offline self-hosting, and have a mature, auditable password-hashing path. Avoid pretending a new email-only login system is higher assurance.

Implementation:

1. Create local credentials only for a pending invite / first admin / audited administrator action. **No public registration.** A local invite enrollment uses a high-entropy, one-use, expiring link, followed by the user setting their **own** password. Never send the user a permanent password chosen by the admin.
2. **Invitation ≠ automatically verified email.** If SMTP verification is unavailable, the invite token demonstrates possession of the delivery channel, not independently verified email ownership. Label the email unverified unless a trusted verification mechanism exists. For production enrollment require a verified channel, explicit admin confirmation of recipient identity, or corporate SSO enrollment, according to policy. Document secure manual invitation delivery for offline installs.
3. Store passwords using maintained Argon2id library, unique salt per credential, well-calibrated memory/work factors (at least OWASP's current floor), secure password checks, and redaction. Single-factor passphrases should follow NIST guidance (at least 15 characters; permit 64+; block common/compromised choices; no arbitrary composition requirements). Keep rate limiting and consistent errors for account enumeration resistance.
4. Implement TOTP enrollment with a maintained library: generate secret securely, deliver QR/URI only in an already authenticated enrollment context, verify a TOTP before activation, store the secret encrypted with a server secret stored outside DB, prevent replay within accepted time steps, and provide one-time hashed recovery codes. Protect reset/re-enrollment with fresh authentication or a controlled admin recovery flow and audit it. MFA enrollment should be required before an admin account can perform privileged actions or a local user can place/cancel live orders.
5. A password reset cannot be a security backdoor. If no email sender is configured, use audited administrator-issued one-time reset/enrollment links and/or the host CLI; if SMTP is configured, use expiring one-use reset links. Recovery must not silently remove MFA requirements. Existing sessions should be revoked on credential recovery/change.
6. External OIDC-authenticated users are *not* given a local password automatically. Explicit linking/enrollment needs a securely authenticated step and audit trail.
7. Handle brute force against both username and account/IP/network, with appropriate privacy/denial-of-service considerations. Do not reveal whether the email is registered. Avoid making DB hash computation an easy unbounded DoS vector.

**Do not market local password+TOTP as a guarantee of regulatory approval or phishing resistance.** For financial institutions, recommend SSO with an explicitly enforced MFA/phishing-resistant policy, access reviews, and operational controls.

## 6. External OIDC (no Authentik dependency)

Preserve/reuse the working OIDC core of PR #30; adapt it to be provider-neutral and optional.

- Use **Authorization Code flow + PKCE (S256)**, with transaction-specific `state`, `nonce`, secure state storage, exact registered callback URIs, and an established OIDC client library. Do not use the implicit flow or password grant.
- Validate discovery/JWKS via trusted HTTPS issuer, token signature/alg, exact issuer, audience/client ID, expiry, nonce, and code/transaction binding. Do not accept an attacker-supplied issuer/client configuration through the login request; prevent mix-up and redirect attacks.
- For providers that provide a **trustworthy `email_verified=true`** claim, the verified email may be used *once* to match a pending invite; this must be backed by a validated OIDC login and possession of the invitation token. **Do not create or attach a user on merely successful OIDC login.** Once linked, re-login uses `(iss, sub)` and active status.
- **Corporate IdP compatibility:** Microsoft Entra ID's `email` claim is optional, mutable and not guaranteed correct; ordinary Entra ID tokens cannot be assumed to contain `email_verified`. Do not set that flag yourself or treat `upn`, domain matching or tenant membership alone as proof of mailbox ownership. For a provider without an acceptable verified-email claim, implement an **admin-approved binding flow**: the invitee opens a valid invitation, completes validated OIDC authentication, and OpenOMS stores a short-lived pending link request containing only the authenticated `(issuer, subject)`, provider/tenant metadata and untrusted display email. An authorized admin compares this with the intended recipient by an independent organizational verification process and explicitly approves or rejects; approval atomically binds that exact identity to the invited principal and records an audit event. The pending identity has no OMS access until approval. If this review flow cannot be completed in the initial slice, fail closed for those invite claims and document this limitation; **do not claim full Entra invite support** until it works.
- Where the organization can provision the user's immutable issuer/subject directly via an independently verified admin workflow, an explicit pre-binding is an alternative to pending approval. Never infer this binding from a mutable display email.
- Do not automatically link a second provider account to an existing principal just because email matches. Link only through a deliberate, authenticated **Connect another sign-in method** flow or an audited admin-initiated invitation flow that independently verifies the new identity.
- V1 may support **one OIDC issuer configured** via `oms.toml` plus secret environment variable. UI says “Sign in with SSO”; the issuer may itself be Entra, Google, Okta, Authentik or Keycloak. Do not imply Microsoft + Google simultaneously work if only one issuer is supported.
- V2 may support **multiple named OIDC configurations** using the same generic adapter, one issuer/client config per button, with transaction/provider binding, and no automatic account linking. Then the UI can display only the configured brand buttons. Do not build V2 into V1 merely to match a mockup.
- Consider enterprise tenant restrictions, especially Microsoft Entra multi-tenant issuers. Do not use arbitrary Microsoft tenant/common logins without explicit issuer/tenant validation. Google domain hints such as `hd` are not authorization rules. Document tested issuers.
- Show provider failure clearly and safely. **Do not silently downgrade to an insecure login mode** when OIDC becomes unreachable. Existing logged-in sessions and running orders follow independently defined operational policies; authentication failure must not stop FIX order routing, but must not bypass permission checks.

## 7. Invitation state machine and user experience

Reuse PR #30's invite hashing, seven-day expiry, revoke/reissue, auditing and case-insensitive target email comparison where correct. Strengthen these conditions:

```text
PENDING -> ACCEPTED | EXPIRED | REVOKED | SUPERSEDED
```

**Admin flow:** Cockpit → Users → Invite → enter email, select role (PM / Trader / Viewer), choose portfolio grants → create one-time invitation URL → copy/send securely. No admin-side second IdP console or manual setting of `email_verified`. Support revoke and reissue. Do not email permanent credentials.

**Recipient flow:** open invitation → see OpenOMS name, intended role and scoped access → choose an *enabled* sign-in method → authenticate → validate invitation and identity → consume invitation and link/create exactly one principal **atomically** → establish session → route to `/trade/` or an appropriate page. Pending invites grant no API access before acceptance.

- OIDC invitation claiming by email requires a valid, trustworthy `email_verified=true` and matching invited email, plus the invitation token and transaction binding. Otherwise require independent **admin approval of the authenticated `(issuer, subject)`** (see §6); no silent “email_verified” override or automatic claim on unverified corporate email. The token alone must not bypass whichever proof route is configured.
- Local password acceptance: securely deliver a link, set password, enroll required MFA; if no independently validated email ownership exists, treat it honestly as invite-link-based enrollment, **not** as verified-email OIDC. Enforce stated deployment/security policy.
- If an invitation targets an existing user to link a new provider, require an already authenticated authorized owner/session or equivalent independent proof. Do not let a mailbox invite overwrite or seize another person's account.
- Invitations should be single use, strongly random, stored hashed, expire, be revocable, and never appear in analytics/logs/referrer leaks. Clear URL tokens after validating/exchanging; set `Referrer-Policy: no-referrer` for sensitive routes and `Cache-Control: no-store`.
- A mismatch, expired token, disabled account, or second identity with matching email produces an explicit safe failure page and **no unauthorized account/grants**. A failed claim should not consume a valid pending invitation.
- Handle simultaneous claims transactionally, return one success; all others fail. Defend login CSRF/session fixation by binding the invite to the login transaction without forwarding the raw token to the external IdP.

## 8. Authorization, machine identities, and audit: preserve these

This is a trading system: **identity changes must not unintentionally weaken order permissions.**

- Retain one server-side `authz` check for every position, order, allocation, strategy, portfolio, user-admin, limit, API and event/stream path. Default deny, check portfolio scope and active status. A Trader may cancel only permitted *own* orders; PM may act on all orders only where granted. Preserve the PR's target-portfolio allocation checks.
- Do not turn existing API keys or strategy/FIX/colo credentials into user-facing password accounts. Machine identities must retain independent scope, expiry/revocation and audit, with a responsible human where already modeled.
- Disabling a human revokes interactive sessions and credentials according to policy on the **next request**; changing a role/grant must not leave a stale long-lived authorization snapshot. Review and fix PR's reported **up to 15-second stream entitlement lag** for sensitive trade events, preferably by invalidating active streams when authorization changes (or justify and document a strict, tested maximum).
- PR #30 reports that already-running strategies keep trading after the responsible human is disabled. Treat this as an explicit **risk-policy decision**, not a silent side effect of auth migration. Do not auto-stop live trading as an incidental auth refactor; design a separately reviewed strategy ownership/escalation/kill-switch policy.
- Audit: bootstrap, invite issued/claimed/revoked/reissued, authentication success/failure, local credential/MFA enrollment/reset, OIDC identity link/unlink, session revoke, user disable, role/portfolio change, and admin recovery. Capture actor ID, target, time, action, result, relevant before/after values; never the secrets themselves.

## 9. Configuration and deployment contract

Fit existing `oms.toml`/environment conventions. **The snippet below is an illustrative target, not a verified current parser schema.** Choose actual config names during implementation, document migrations, and keep backward compatibility as appropriate.

```toml
[auth]
registration = "invite_only"

[auth.local]
enabled = true
require_mfa_for_admin = true
require_mfa_for_live_trading = true

[auth.oidc]
enabled = false
# issuer = "https://login.example.com/realms/company"
# client_id = "openoms"
# public_base_url = "https://oms.example.com"
# client secret from OMS_OIDC_CLIENT_SECRET

[auth.session]
idle_ttl_minutes = 30
absolute_ttl_hours = 12
```

- **Default:** no external IdP, no SMTP, no public registration; first admin can log in; local invites can be accepted securely using documented manual delivery. Require TLS beyond loopback and MFA for sensitive use.
- **Enterprise:** set OIDC issuer/client + secret, verify callback and trust; choose `sso_only` policy when appropriate, with documented host-only break-glass recovery. Do not leave an unprotected local admin backdoor.
- **Optional development recipes:** keep a clearly marked, opt-in `docker compose --profile oidc ...` example for Authentik or Keycloak. It is a test fixture, **not** a production runtime dependency.
- **No public secrets:** `oms-dev-secret` and other known development credentials must not reach a deployed service. Remove production use of published defaults; rotate exposed secrets and fail fast for missing production credentials.
- No forced separate IdP DB/service in the normal deploy stack. Ensure install, Docker, test server, Tailscale/reverse proxy, docs and health checks agree.
- Do not let a UI setting advertise “SSO is enabled” if provider discovery/callback cannot function. Expose usable diagnostic state without leaking secrets.

## 10. Ordered implementation slices (small PRs/commits)

### Slice A — baseline and invariants
- [ ] Inspect PR #30, migration numbering, auth middleware and tests.
- [ ] Record working vs failing checks; keep PR's roles/invite/strategy code.
- [ ] Add regression tests for authorization and account-linking invariants *before* changing auth plumbing.

### Slice B — unified internal account/session
- [ ] Convert shared cockpit administrator to a named human admin through a backward-compatible transition.
- [ ] Use one principal/session abstraction for `/cockpit/` and `/trade/`.
- [ ] Preserve roles, portfolio and API-key behavior; implement complete session/CSRF protections.

### Slice C — local account lifecycle
- [ ] Implement local credentials with maintained password-hashing library.
- [ ] Implement secure first-admin enrollment, invite redemption and password setup.
- [ ] Implement TOTP + recovery/reset lifecycle and enforce MFA for sensitive local actions.
- [ ] Implement logout, session revoke, disable, account recovery and comprehensive tests.

### Slice D — make OIDC provider-neutral
- [ ] Remove Authentik as a required dev/deploy/runtime dependency.
- [ ] Preserve generic OIDC and optional external Authentik/Keycloak compatibility.
- [ ] Connect invite acceptance to same internal user/session model.
- [ ] Implement correct state/nonce/PKCE/issuer validation, trustworthy verified-email claims and the explicit admin-review binding path needed by Entra, plus safe manual identity linking.
- [ ] Add provider diagnostics and configuration docs.

### Slice E — UI/onboarding and release hardening
- [ ] One login page, with only configured methods; one Users/Invite workflow.
- [ ] Clearly show invite role/portfolio scope and robust access/error pages.
- [ ] Update installer, compose, dev scripts, docs, architecture diagrams and migration notes.
- [ ] Browser E2E tests with real OIDC provider and local login; run all project CI checks.
- [ ] Security pass: secrets/logs, all authz endpoints, CSRF, cookie flags, OTP/TOTP, IdP outage, session revocation, invite races.

**Do not collapse all slices into an enormous unreviewable commit.** Use existing repository commit conventions and keep operational changes separable from authz changes.

## 11. Mandatory test matrix / definition of done

Write unit, DB integration, and browser E2E tests as appropriate. At minimum:

| Case | Expected result |
|---|---|
| Fresh install; no Authentik/SMTP | Named admin can sign in and use cockpit |
| Bootstrap endpoint on externally reachable host | Cannot claim admin without host-authorized bootstrap |
| Invite new local Trader | One account, correct role/grants, password/MFA enrollment |
| Invite new OIDC Trader with verified matching email | One account, correct grants, valid session |
| Valid invite with unverified/different OIDC email | No automatic access; email-claim path refused, pending invite preserved |
| Entra ID without trustworthy email_verified | Approved only after independent admin review of exact issuer + subject; otherwise denied |
| Uninvited OIDC user | Denied; no auto-provisioned trading access |
| Same email from another OIDC issuer | Not linked/merged automatically |
| Reused, expired, revoked, or raced invite | Denied; no duplicate identities or grants |
| Trader calls order/portfolio UUID outside grant | 403/404; no data leaks |
| Trader cancels another Trader's order | Denied even in same portfolio |
| Viewer submits or cancels order | Denied |
| PM accesses ungranted portfolio/limits | Denied |
| Disabled human has open HTTP + stream sessions | Subsequent access denied and stream revoked under tested policy |
| Change portfolio grant/role | Server-side next-request checks use updated permissions |
| Password-only local login attempts sensitive action | Denied until required MFA is satisfied |
| Admin resets local credential/MFA | Explicit recovery policy, sessions revoked, audit exists |
| OIDC invalid `state`, `nonce`, signature, issuer, audience or PKCE | Denied, no session/identity changes |
| OIDC IdP unavailable | Login fails safely, no insecure fallback; OMS order engine remains operational |
| Missing/invalid CSRF or hostile websocket Origin | Blocked |
| Login/logout or privilege change | Session fixation prevented; old sessions invalidated as designed |
| API key / STRATEGY / FIX / colo flows | Preserve current behavior and authorization |
| Production config with public dev secrets | Rejected or clearly blocked from production deployment |
| Host-only admin recovery | Audited and access controlled; no internet-exposed bypass |

**Run documented checks against isolated databases only**, respecting the existing plan's warning to skip destructive `real_docker` tests against shared state:

```bash
cargo test --workspace
# Follow the repository's exact isolated-Postgres integration test instructions.
# If running ignored tests, do not target the shared OMS database.
(cd cockpit && npm test && npm run build)
(cd docs-site && npm run check)
```

Add browser E2E tests to prove a complete invite -> login -> trader UI sequence, both local and OIDC, plus denied/mismatched login scenarios. Test under HTTPS through a real reverse proxy (including expected cookie attributes), not only localhost.

## 12. Output required from the agent

When finished, report:

1. **Implemented architectural decision** and any deviations from this plan, with security reasoning.
2. **Files/migrations/commands changed**, including how legacy single-admin and existing OIDC identities are migrated without lockout or unintended permissions.
3. **Supported authentication modes** (and what is intentionally unsupported: email OTP, native GitHub, multiple OIDC issuers if not implemented).
4. **User walkthroughs:** local fresh install, inviting a tester without SSO, configuring corporate OIDC, switching to SSO-only, admin account recovery.
5. **Test results:** commands, counts, failures/untested paths, and manual E2E results.
6. **Security caveats and explicit unresolved decisions**, notably existing strategy-running behavior after owner disable, stream entitlement refresh, and production MFA/IdP policy.

If a requirement conflicts with current code, preserve secure existing behavior and document the conflict instead of improvising an unsafe workaround. Do not claim that code or security tests passed if they were not actually run.

## 13. Primary references

**Project-derived**

- [OpenOMS repository](https://github.com/maxkuttner/openoms)
- [PR #30 — invite-only users, roles, Authentik SSO](https://github.com/maxkuttner/openoms/pull/30)
- [PR branch design specification](https://github.com/maxkuttner/openoms/blob/feature/users-roles-sso/docs/superpowers/specs/2026-11-13-users-roles-sso-design.md)
- [PR branch existing implementation plan](https://github.com/maxkuttner/openoms/blob/feature/users-roles-sso/docs/superpowers/plans/2026-11-13-users-roles-sso.md)
- [Current login documentation on main](https://github.com/maxkuttner/openoms/blob/main/docs-site/src/content/docs/configuration/login.md)
- Uploaded **Current sign-in setup.html** supplied by the owner, describing Authentik vs Direct UX; not assumed to be exact source code.

**Security and protocol guidance**

- [NIST SP 800-63B-4 (2025) — authenticator requirements](https://pages.nist.gov/800-63-4/sp800-63b/authenticators/) — explicitly excludes email as out-of-band authentication; gives password requirements.
- [OAuth 2.0 Security Best Current Practice — RFC 9700](https://www.rfc-editor.org/rfc/rfc9700.html)
- [OAuth 2.0 for Browser-Based Applications — RFC 10017](https://www.rfc-editor.org/rfc/rfc10017.html)
- [OWASP Authentication Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Authentication_Cheat_Sheet.html) — secure OIDC account linking.
- [OWASP Password Storage Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html)
- [OWASP Session Management Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html)
- [OWASP CSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)
- [OWASP MFA Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Multifactor_Authentication_Cheat_Sheet.html)
- [Microsoft Entra ID token claim reference](https://learn.microsoft.com/en-us/entra/identity-platform/id-token-claims-reference) — `email` is mutable, may be absent and is not suitable for authorization.

**Design principle:** simplify the *operator's* experience by making the external identity provider optional; do not simplify away strong authentication, revocable sessions, correct invite linking, or server-enforced trading permissions.
