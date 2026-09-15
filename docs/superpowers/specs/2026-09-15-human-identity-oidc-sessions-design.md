# Human identity: OIDC login and sessions

**Date:** 2026-09-15
**Status:** approved design, not yet implemented
**Enables:** a trader GUI (separate spec), named admin operators (separate spec),
maker-checker approval, and per-human attribution on the audit trail.

## Problem

The OMS has one kind of credential: an `api_key` row — `key_id` plus a bcrypt-hashed
secret, resolved by `auth_middleware` (`src/auth.rs:27`) into
`AuthContext { principal_id }`. It never expires, it has no session, and it is
designed to sit in a machine's environment variable.

The cockpit has something else entirely: one shared static string compared by
`admin_middleware` (`src/auth.rs:77`). It carries no identity at all — the console
cannot say who is logged in, only that someone knew the password.

Neither is an authentication story for a human at a desk. A trader GUI needs a
login, and there is nothing to log in with:

1. **Credential shape.** An API token is a permanent bearer credential. The cockpit
   already parks its admin token in `localStorage` (`cockpit/src/api/client.ts:11`) —
   a deliberate trade-off for a single-operator console on loopback. Handing traders
   a never-expiring order-entry credential to keep in browser storage is a different
   risk class.
2. **Nothing to authenticate against.** There is no password, no federation, no
   human credential of any kind in the schema.
3. **No session lifetime.** `api_key.revoked_at` is the only lever. Keys do not
   expire; browser sessions must.
4. **No per-human attribution.** Two people sharing one principal's token are
   indistinguishable in the event log. The per-order audit trail shipped on
   2026-09-15 makes this gap visible: `actor` can say `"oms"` or `"binance"`, but
   never *which person*.

## What already anticipates this

The identity model was designed for federated humans and never wired up:

- `principal.principal_type` permits `'HUMAN'` (migration 0001).
- `principal.external_subject` is `TEXT UNIQUE`, exposed on `CreatePrincipal` /
  `UpdatePrincipal`, filterable via `PrincipalFilter` (`src/admin.rs:153`), and
  labelled in the cockpit form as **"External subject (OIDC sub, optional)"**
  (`cockpit/src/pages/Principals.tsx:90`).
- `principal_portfolio_grant` already keys entitlements on `principal_id`, so a
  human needs no new entity to be authorised.
- `broker_connection.credentials_updated_by` is commented
  `-- reserved; null until user accounts exist` (migration 0021).

Nothing authenticates against any of it. This spec builds the missing front door,
not a new identity model.

## What we are building

OIDC login that mints a server-side session, resolving to the same
`AuthContext { principal_id }` that API tokens already produce.

The OMS acts as a confidential OIDC client: authorization code with PKCE, it
validates the ID token, matches the `sub` claim to `principal.external_subject`, and
sets an httpOnly session cookie. Tokens from the identity provider never reach the
browser.

**Tokens remain the machine story; sessions become the human story. They differ only
at the door.**

### Why not the alternatives

**SPA-side PKCE with the IdP's ID token as a bearer** needs no session store, no
cookies and no CSRF handling — but the token lives in browser JavaScript, which is
the exact exposure that makes the current model wrong for traders, and it cannot be
revoked before it expires. For a system that sends orders, waiting out an expiry is
not an acceptable answer to "revoke this person now".

**The OMS minting its own JWT** after login gives stateless verification, at the cost
of running a signing key and its rotation, still no early revocation, and a second
token system beside `api_key` for no gain.

Server-side sessions cost one indexed lookup per request and CSRF handling, and buy
instant revocation.

### No password storage, ever

Credentials stay with the identity provider. MFA, rotation, lockout, offboarding and
password policy are the IdP's job — which is what a desk expects, and what this
project should not reimplement worse.

## Out of scope

- **The trader GUI.** Its own spec, consuming `/auth/me` and the session cookie.
- **Named admin operators.** The admin surface keeps its single shared token. When
  that spec comes, `broker_connection.credentials_updated_by` is where it plugs in.
- **Local password authentication.** No `password_hash` column, now or later.
- **Storing IdP access or refresh tokens.** See "Token handling" below.
- **Sealing the OIDC client secret in the credential store.** It is read from
  `OMS_OIDC_CLIENT_SECRET` instead (see "Client registration and key material").
  Giving it a home in the sealed store is possible follow-up work, not done here.

## Design

### Session storage

```sql
CREATE TABLE user_session (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    principal_id        UUID NOT NULL REFERENCES principal(id),
    token_hash          TEXT NOT NULL UNIQUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    absolute_expires_at TIMESTAMPTZ NOT NULL,
    revoked_at          TIMESTAMPTZ,
    user_agent          TEXT,
    ip                  INET
);
CREATE INDEX ON user_session (token_hash) WHERE revoked_at IS NULL;
```

The cookie value is 256 bits from `rand`, base64url-encoded, stored as its SHA-256.

**This deliberately diverges from `api_key`, which bcrypts its secret**
(`generate_key_material`, used by `create_trading_token` at `src/admin.rs:1992`).
bcrypt exists to slow the guessing of low-entropy secrets and is affordable at
API-call rates; a session cookie is verified on every interaction and its value is
already full-entropy random. A single indexed lookup is the correct cost. The
migration carries a comment saying so, because the divergence from the neighbouring
table is intentional.

### Session lifetime and revocation

Two clocks:

- **Idle timeout**, sliding, default 30 minutes, tracked by `last_seen_at`.
- **Absolute cap**, default 12 hours, in `absolute_expires_at`, so a desk
  re-authenticates at least daily.

Both configurable. `last_seen_at` is written on use, throttled to at most once a
minute so a polling blotter does not turn every request into a write.

Revocation has three routes: `revoked_at` on one session; an admin endpoint killing
every session for a principal; and a principal flipped to `DISABLED`, which
invalidates its sessions on their next request — the same `p.status = 'ACTIVE'` join
`verify_key` already performs. Expired rows are swept periodically, following the
pattern in `src/expiry.rs`.

### Cookie attributes

`__Host-oms_session`, `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`.

`Secure` and the `__Host-` prefix require HTTPS, which plain-http localhost cannot
satisfy. On a loopback bind the cookie is named `oms_session` and carries neither the
prefix nor `Secure`; on any other bind it is `__Host-oms_session` with both, and the
server refuses to start if it cannot set them. This is the same reasoning that makes
`main.rs:803` refuse a default admin password off-loopback.

CSRF defence is `SameSite=Lax` plus an `Origin` / `Sec-Fetch-Site` check on every
state-changing method. No CSRF token, no double-submit cookie.

### Endpoints

| Route | Purpose |
| --- | --- |
| `GET /auth/login` | Generate `state`, `nonce`, PKCE verifier; 302 to the IdP |
| `GET /auth/callback` | Validate, exchange, resolve principal, set session cookie |
| `POST /auth/logout` | Revoke the session, clear the cookie |
| `GET /auth/me` | The authenticated principal and its grants |

`/auth/logout` is a **local logout**: it revokes our session and clears our cookie,
leaving the IdP session alone, so the user stays signed in to other applications.
RP-initiated logout — redirecting to the provider's `end_session_endpoint` — is a
later option, not part of this spec.

These form a fourth router, outside both existing middleware layers, since they are
unauthenticated by definition.

### The flow

`/auth/login` generates `state`, `nonce` and a PKCE verifier and redirects. Those
three must survive the round trip; they live in a short-lived (5 minute) httpOnly
cookie rather than a table — they are per-browser state, worthless after the
callback, and a table would need its own sweep.

`/auth/callback` checks `state`, exchanges the code, validates the ID token
(signature, `iss`, `aud`, `exp` with small clock leeway, `nonce`), and takes `sub`.
`state` and `nonce` are single-use, so a replayed callback fails.

Scopes are `openid profile email`. `sub` is the key; name and email are for display
only.

### Client registration and key material

The OMS is a confidential client. Issuer URL, client id and scopes are plain config.

**Correction (implementation, 2026-09-16):** this section originally specified that
the client secret goes into the sealed credential store (`secrets.rs::seal`,
AES-256-GCM with an AAD), written through the `oms config` credential maintenance
command, on the reasoning that the secret should not be recoverable out of a
database dump. That store is keyed by broker/feed connection row; it has no schema
or write path for a single OIDC client secret, and building one is its own piece of
work, not a byproduct of this task. **The implementation instead reads the secret
from the `OMS_OIDC_CLIENT_SECRET` environment variable**, never written to
`oms.toml` or to any table. This is bootstrap-tier, the same tier as
`OMS_ADMIN_PASSWORD`, `POSTGRES_PASSWORD` and `OMS_PASSWORD`, all of which already
live in `.env` rather than the sealed store — and it still satisfies this section's
original reason for sealing the secret: it never reaches the database, so it cannot
appear in a dump. Moving it into the sealed store later, once that schema and write
path exist, is possible follow-up work; see "Out of scope" below.

Discovery (`{issuer}/.well-known/openid-configuration`) is fetched lazily and cached.
JWKS is cached and refetched on an unknown `kid`, rate-limited so a malformed token
cannot trigger a fetch storm.

### Token handling

**IdP access and refresh tokens are discarded** the moment identity is established.
The session is ours and so is its lifetime. There is no refresh-token vault to
protect, and — operationally significant — **the IdP is a dependency of login, not of
every request.** If it goes down mid-session, trading continues.

### Implementation note on validation

Hand-rolled ID-token validation is a well-known source of security bugs: skipped
signature checks, unvalidated `aud`, accepting `alg: none`. Use a maintained OIDC
client crate rather than writing discovery, JWKS handling and validation by hand.

**Pinned: `openidconnect` 4.x** (`ramosbugs/openidconnect-rs`), used for exactly three
things — provider discovery, the code-for-token exchange, and ID-token verification.
It is the de-facto standard in this space and the crate the Rust OIDC ecosystem is
built on top of, with roughly four million recent downloads.

Checked on 2026-09-15: 4.0.1 released 2025-07-06, last repository activity
2025-11-08, 641 stars, 76 open issues, not archived. Adoption is strong but the pace
has slowed — no release in about fourteen months. That is a flag on a
security-critical dependency, not a blocker: OIDC is a stable protocol and the
download volume means defects surface. Re-check before implementation starts if that
is more than a few months from now.

**The wrapper crates are deliberately rejected.** `axum-oidc` and similar bring their
own session and middleware model, which would fight the design chosen here — our
session lives in Postgres, is ours to revoke, and is produced by our own middleware.
We want the protocol library, not a framework.

The risk is contained by a boundary the design already draws for other reasons: the
validator takes its keys as an argument, with discovery and JWKS caching outside it.
That seam exists to make the security-critical half testable without a network, and
it doubles as the swap point if this dependency ever needs replacing.

Whatever is chosen, **the validator must accept its keys as an argument rather than
fetching them.** Discovery and JWKS caching sit outside it. That is what makes the
security-critical half testable with no IdP and no network.

### Principal resolution

```sql
SELECT id FROM principal
WHERE external_subject = $1 AND status = 'ACTIVE' AND principal_type = 'HUMAN'
```

Two invariants fall out of that predicate, both worth keeping: a `SERVICE` or
`STRATEGY` principal can never hold a browser session, and a disabled principal's
session dies on its next request.

### Unknown `sub`: just-in-time provisioning

An unrecognised `sub` **creates a `HUMAN` principal with zero grants.**

A principal without grants can do nothing: `require_order_grant` gates every order
path and `/portfolios` returns only granted rows. First login therefore produces an
authenticated identity that can see nothing until an admin grants it a portfolio.
The alternative — refusing login until an admin hand-copies opaque `sub` strings into
the form at `Principals.tsx:90` — is miserable and typo-prone at desk scale.

Two guards:

- An **optional required claim** (group or role), so only the intended slice of an
  IdP tenant provisions at all.
- `display_name` and email captured from the token, so an admin granting portfolios
  sees a name rather than a UUID.

### Authorization is unchanged

This is the load-bearing claim of the design: **a session and an API token carrying
the same `principal_id` have identical powers.** Grants, risk limits,
`require_order_grant`, blotter scoping — none of it learns that sessions exist.

`auth.rs` grows a `session_middleware`, and `auth_middleware` becomes a combined
front: try the session cookie, fall back to Basic/Bearer key material, inject
`AuthContext` either way. One middleware, one route tree, both credential types on
the same handlers. The GUI does not get a parallel route tree.

### Audit trail

`actor` on order events currently holds `"oms"` for anything the OMS decided and the
broker's name for anything it reported. With human identity that field can finally
answer *who*:

- An **authenticated command** stamps `actor` with the acting principal's `code`.
- `"oms"` is reserved for **system-generated** events — expiry sweeps,
  reconciliation.
- **Broker-driven** events keep the broker's name.

This applies to token-authenticated commands too, not only sessions. Today a
token-submitted order records `"oms"`, discarding information the request already
carried. **This changes existing behaviour** and is called out deliberately rather
than slipped in.

`AuthContext` grows from bare `principal_id` to carrying the principal's `code`, so
handlers need no extra query to stamp an event. `correlation_id` and `causation_id`
are left alone — they are earmarked for command correlation and should not be quietly
repurposed as session fields.

### Configuration and first run

A `[auth.oidc]` block in `oms.toml`: issuer, client id, scopes, optional required
claim, session idle and absolute TTLs, and the OMS's own public base URL — from which
the redirect URI is derived as `{public_base_url}/auth/callback` and against which the
callback's `Origin` is checked, so there is one value to configure and one value
registered at the IdP. The client secret is not in the file — it comes from the
`OMS_OIDC_CLIENT_SECRET` environment variable (see "Client registration and key
material" above for why).

**Off by default.** With no `[auth.oidc]` block, the OMS behaves exactly as it does
today: `/auth/*` returns 404 and nothing else changes. The
`curl | sh` → `oms database init` → `oms` first run is untouched.

Enabling OIDC on a non-loopback bind without HTTPS-capable cookie settings refuses to
start, mirroring the existing rule for a default admin password.

## Testing

### Pure unit tests

In-module `#[cfg(test)]`, matching the rest of the codebase.

- **ID-token validation** against a key pair generated in the test: a valid token
  passes; expired, wrong `aud`, wrong `iss`, bad signature, `alg: none` and wrong
  `nonce` each fail. Every one of these is a real-world OIDC vulnerability; this is
  the set that matters most.
- **Cookie attributes:** loopback drops `Secure` and the `__Host-` prefix; any other
  bind requires both.
- **Session lifetime:** idle slide versus absolute cap, whichever expires first
  wins; `last_seen_at` writes throttled to once a minute.
- **CSRF:** same-origin allowed, cross-site rejected, missing origin on a
  state-changing method rejected.
- **`state` / `nonce` / PKCE:** shape, verifier-to-challenge derivation, single use.
- **Callback errors:** `access_denied`, `state` mismatch, missing cookie — each
  yields the right status and never a session.
- **Required-claim gate:** present, absent, wrong type.
- **`actor` derivation:** principal code, `"oms"`, broker name.

### Postgres tests

`#[ignore]`d, run with `cargo test -- --ignored`, following
`applying_twice_is_a_no_op` and the `load_stream` round-trip.

- Create → look up → touch → revoke → lookup fails.
- A principal flipped to `DISABLED` kills its session.
- Absolute expiry.
- JIT provisioning is idempotent: repeated logins with one `sub` yield exactly one
  principal.

### End-to-end

A Keycloak service behind a docker-compose profile, driving a real discovery and code
exchange. The repo already ships a compose file for Postgres, so a second profile is
the idiomatic home. Kept out of the default CI path and run deliberately.

No frontend tests — the cockpit has none today, and `/auth/me` is consumed by the GUI
spec rather than this one.

## Failure modes

| Condition | Result |
| --- | --- |
| IdP returns `error=access_denied` | Login page with a message; no session |
| `state` mismatch or missing cookie | 400; no session |
| ID token fails validation | 401, logged with the reason; no session |
| IdP unreachable | 503 on login; existing sessions unaffected |
| Unknown `sub` | JIT-provisioned principal with zero grants |
| Unknown `sub`, required claim absent | 403; no principal created |
| Session idle or absolutely expired | 401; cookie cleared |
| Principal set to `DISABLED` | 401 on next request |
