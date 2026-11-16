---
title: Sign-in and users
description: The first account becomes the admin, people sign up (and wait for approval) or are added by an admin, an authenticator app is optional, and single sign-on with your own identity provider can create or link accounts.
---

Everyone signs in the same way, to the cockpit (`/cockpit/`) and to the trading app
(`/trade/`): one session, one account per person. The OMS has its own accounts (a password
plus, optionally, an authenticator app), so a fresh install needs no identity provider, mail server or
extra container. If your organisation has single sign-on (SSO), you can add it later, next
to local accounts or instead of them.

Accounts work the way [Open WebUI](https://docs.openwebui.com/)'s do: the first account
signed up is the admin; after that people sign up themselves and wait for an admin to approve
them, or an admin adds them directly. Each account has a role: **admin**, **user** or
**pending**.

## First start: create the admin account

On a fresh install (no admin exists yet) the sign-in page shows **Create the admin account**:
name, email and a password. That account gets the admin capability (the admin console) and
nothing else: it cannot place orders until it is given a portfolio role and portfolios (see
[Roles](#roles-and-permissions)). If two people submit the form at the same moment, exactly one
becomes the admin; the other is an ordinary sign-up.

`oms init` does not ask for an admin any more. For scripted installs,
`oms auth bootstrap-admin --email you@example.com --name "Your Name"` creates an admin and
prints a generated password once (only while no active admin exists, or with
`--force-recovery`).

## Sign-up and approval

```toml
[auth]
enable_signup = true          # default: the sign-in page offers "Sign up"
default_user_role = "pending" # default; "user" activates sign-ups at once
```

A **pending** account can sign in, but sees only "Your account is waiting for an administrator
to approve it." Every API call except `GET /auth/me`, `GET /auth/methods` and
`POST /auth/logout` answers `403 {"error":"approval_pending"}`. Admins find pending accounts at
the top of the **Users** page: **Approve** (choose a portfolio role and portfolios) or
**Reject** (the account is disabled). With `default_user_role = "user"` a sign-up is active at
once, but with no portfolio access until an admin grants some.

With `enable_signup = false` there is no "Sign up" link and `POST /auth/local/signup` answers
`403 signup_disabled` (the very first account, the admin, is still allowed). Sign-ups are
rate-limited like logins, follow the password policy, and are audited.

## Add a user

On the **Users** page, **Add user** creates a local account directly: name, email, a password
and a role (admin, user or pending). Pass the password on yourself, by a channel you trust; the
user can change it under **Account**. The API is `POST /admin/users` with
`{"name", "email", "password", "role"}`.

From the list you can also change a user's **portfolio role** and **portfolios** (effective on
the next request), grant or remove **admin**, **disable** a user (sessions end at once and API
keys stop), and create or revoke their **API keys**. Revoking access ends the user's open
live-update connections immediately; changes the running server is not told about (a host-CLI
`oms auth recover-admin`, a session reaching its absolute lifetime) close them within 15
seconds.

## Local accounts

- **Password.** 15 to 128 characters, no composition rules, not a common password. Stored as
  an Argon2id hash. Change it under **Account** in either app (`POST /auth/local/password`).
- **Authenticator app (TOTP), optional.** Standard 6-digit codes, renewed every 30 seconds.
  Turn it on (or off, with a current code) under **Account → Two-step sign-in**. Once it is
  on, every sign-in asks for a code after the password. The secret is sealed with the
  [master key](/operations/master-key-and-backups/).
- **Recovery codes.** Ten single-use codes, shown once at enrolment, each standing in for
  one authenticator code.
- **Rate limits.** More than 5 failed attempts per account, or 20 per client address, in
  15 minutes are refused, and 10 failures in a row lock the account for 15 minutes. Every
  failure looks the same, so a response never reveals whether an email has an account.

### Two-step sign-in (MFA)

By default MFA is **optional**: a password sign-in is enough for someone without an
authenticator app, and someone who has one must enter its code to sign in. To require it of
everyone:

```toml
[auth.local]
require_mfa = true
```

Then every local account sets up an authenticator app at its first sign-in, it cannot be turned off, and a session that signed in with a password but has not
passed the authenticator check can read, and nothing else. A session that passed MFA is
needed for:

- submitting, cancelling or allocating orders;
- creating or handing over strategies, and changing limits;
- every change in the admin console (users, roles, connections, keys).

Reads never need it, and neither does adding or removing instruments on your own
watchlist: that is a personal view setting, not an order or admin action. API keys, strategy tokens and the host token are exempt: they are
machine credentials with their own scope. For SSO sessions see
[`require_amr`](#require-mfa-from-the-identity-provider).

## Roles and permissions

Each user has one portfolio role. The backend checks every request against the role's permissions,
inside the portfolios the user has been given: *view* puts a portfolio in scope, *trade*
allows creating and cancelling there.

The account role (admin / user / pending) says whether someone may use the OMS and the admin
console. What a **user** may do with orders comes from their **portfolio role** and the
portfolios they have access to:

| Permission | PM | Trader | Viewer |
|---|---|---|---|
| View own orders (`orders.view_own`) | yes | yes | yes |
| View all orders in the portfolio (`orders.view_all`) | yes | | yes |
| Create orders (`orders.create`) | yes | yes | |
| Cancel own orders (`orders.cancel_own`) | yes | yes | |
| Cancel any order (`orders.cancel_any`) | yes | | |
| View positions (`positions.view`) | yes | yes | yes |
| View limits (`limits.view`) | yes | yes | yes |
| Set limits (`limits.manage`) | yes | | |
| Manage strategies (`strategies.manage`) | yes | | |

"Own" means submitted by that user. A fourth role, **strategy**, is for automated
[strategies](/strategies/fix/) and cannot be given to a person. **Admin** is not a portfolio
role: it is a switch on the Users page that opens the admin console. The last active admin cannot be
removed or disabled.

Every strategy has a **responsible user**, shown on the **Strategies** page. Handing a
strategy over is an audited action. If the responsible user is disabled, the strategy is
flagged "needs a new responsible user" and cannot be started until you pick another.

## Add single sign-on

An `[auth.oidc]` block turns on OpenID Connect sign-in. The sign-in page then shows
"Sign in with SSO" beside the local form. One provider is supported at a time.

```toml
[auth]
public_base_url = "https://oms.example.com"   # the address people open in the browser

[auth.oidc]
issuer = "https://login.example.com/realms/company"
client_id = "openoms"
```

Put the client secret in the environment, never in `oms.toml`:

```sh
OMS_OIDC_CLIENT_SECRET=...
```

A `.env` next to `oms.toml` works (see
[Config file locations](/configuration/file-locations/#env-files)). Register the redirect
URI `{public_base_url}/auth/callback` with the provider (a confidential client, authorization
code flow with PKCE), then restart the OMS. The cockpit's **Sign-in** page shows the issuer,
whether discovery works (and why not) and the exact redirect URL.

If the provider is unreachable, SSO stays off for that run and the error is logged. Local
sign-in, order routing and existing sessions are unaffected; the OMS never falls back to a
weaker login.

| Key | Default | Meaning |
|---|---|---|
| `[auth] public_base_url` | | Browser-facing origin. `[auth.oidc].public_base_url` still works but is deprecated and logs a warning. |
| `[auth.local] enabled` | `true` | `false` turns local sign-in off. |
| `[auth] enable_signup` | `true` | Offer "Sign up" (see [Sign-up and approval](#sign-up-and-approval)). |
| `[auth] default_user_role` | `pending` | `pending` or `user`: the role of a new sign-up, local or SSO. |
| `[auth.local] require_mfa` | `false` | `true` makes an authenticator app mandatory for local accounts (see [Two-step sign-in](#two-step-sign-in-mfa)). |
| `[auth.session] idle_ttl_minutes` | `30` | A session ends after this much inactivity. |
| `[auth.session] absolute_ttl_hours` | `12` | A session ends after this regardless. |
| `[auth.oidc] issuer`, `client_id` | required | |
| `[auth.oidc] scopes` | `openid profile email` | |
| `[auth.oidc] required_claim`, `required_claim_value` | none | Also require this claim in the ID token (both keys, e.g. `groups` and `oms-traders`). |
| `[auth.oidc] enable_signup` | `false` | An unknown SSO login creates an account with `default_user_role`. |
| `[auth.oidc] merge_accounts_by_email` | `false` | An unknown SSO login whose **verified** email matches an account is linked to it. See the warning below. |
| `[auth.oidc] trust_email_verified` | `true` | `false`: never trust the provider's email (merging never matches). |
| `[auth.oidc] require_amr` | `[]` | See below. |

### Which SSO logins get in

A login is keyed by the provider's `(issuer, subject)`, never by email. The first time an
identity arrives:

- **Already linked** (an earlier login, or a merge): signed in.
- **`merge_accounts_by_email = true`** and the ID token says `email_verified: true` with an
  address equal (case-insensitive) to an existing account's: the identity is linked to that
  account and signed in. An unverified email never merges.
- **`enable_signup = true`**: a new account is created with `default_user_role` (pending by
  default, so it waits for approval). Its email is recorded only if verified and not already
  taken; it never takes over another account.
- **Neither**: no access ("There is no account for this sign-in"). Nothing is created. Add the
  person with **Add user** and turn on `merge_accounts_by_email`, or turn on `enable_signup`
  and approve them.

:::caution[Merging by email trusts your provider completely]
With `merge_accounts_by_email` on, anyone who can make your identity provider assert a
verified email address gets into the account with that address, admins included. Turn it on
only for a provider you control whose users cannot choose or change their verified email (a
company Entra ID or Google Workspace tenant, for example), never for a public provider where
anyone can register an address.
:::

### Providers

| Provider | Notes |
|---|---|
| Google | Sends `email_verified`. |
| Okta, Keycloak, Authentik | Send `email_verified` when the user's email is verified in the provider. Check that yours does, or rely on approval. |
| Microsoft Entra ID | Does not reliably send `email_verified`, and its `email` or `upn` is not proof of a mailbox, so merging by email rarely matches; use `enable_signup` and approval. Use your tenant's issuer (`https://login.microsoftonline.com/{tenant-id}/v2.0`), never `common`. |

### Require MFA from the identity provider

By default every SSO login counts as MFA-verified: the provider's own policy is trusted. To
insist, list the authentication methods (`amr` values) the ID token must carry:

```toml
[auth.oidc]
require_amr = ["mfa"]
```

A login without them still signs in, but as a session that cannot trade or change admin
settings. Use the values your provider emits (for example `mfa`, `otp`, `hwk`).

## SSO only

```toml
[auth]
policy = "sso_only"
```

Local sign-in and sign-up are refused and the login page shows only SSO, whatever
`[auth.local].enabled` says (`[auth.local] enabled = false` has the same effect). Set this
only after SSO works and an admin has signed in through it (or is linked to it by email).

The host CLI still works, but the passwords `oms auth bootstrap-admin` and
`oms auth recover-admin` print are for **local** sign-in, which this policy refuses; the
command prints a note saying so. To use one, set `policy = "local_and_sso"` and restart, sign
in, then set `sso_only` back and restart.

## Recover an admin from the host

If nobody can sign in as an admin (password, or authenticator and recovery codes, lost; or a
provider outage), recover from the machine the OMS runs on:

```sh
oms auth recover-admin --email you@example.com
```

This gives the admin a new generated password (printed once), removes their authenticator,
and signs them out everywhere. Change the password under **Account** after signing in. It is audited. To create a new admin while one already exists,
add `--force-recovery` to `oms auth bootstrap-admin`.

These commands use a per-boot **host token**, the file `host-token` next to `oms.toml`
(mode 0600, replaced at every start). The server accepts it from loopback addresses only,
so it is no remote door. Under Docker with bridge networking, run them in the container:

```sh
docker exec <container> oms auth recover-admin --email you@example.com
```

## Upgrading from the shared admin password

Earlier versions had one cockpit password (`[server].admin_password` or
`OMS_ADMIN_PASSWORD`). It no longer works: the bearer is refused, and the setting is ignored
with a warning at startup (delete it). If no admin exists yet, open the cockpit and **create
the admin account**. If an admin already exists, sign in as them; if nobody can, use
[`oms auth recover-admin`](#recover-an-admin-from-the-host).

Invites are gone too: pending invite links stop working. Add those people with **Add user**,
or let them sign up and approve them. Existing SSO users keep their login as long as the
`issuer` is unchanged.

## HTTPS beyond localhost

The session cookie is marked `Secure`. Unless both the bind address and `public_base_url`
are loopback, `public_base_url` **must** start with `https://`, and so must the OIDC
issuer; the OMS refuses to start otherwise. The usual setup is a loopback bind with TLS in
front (nginx, Caddy or `tailscale serve`), with `public_base_url` set to the HTTPS address
users open.

## Trying SSO locally

The repository's `docker-compose.yml` has an optional Authentik provider, for development
only:

```sh
docker compose --profile oidc up -d
```

It takes a minute or two on first start. Use `issuer = "http://localhost:9000/application/o/oms/"`,
`client_id = "oms"`, `public_base_url = "http://localhost:3001"` and
`OMS_OIDC_CLIENT_SECRET=oms-dev-secret`. The blueprint creates the provider; create a user
in the Authentik admin UI (`http://localhost:9000/if/admin/`, `akadmin` / `admin`, dev only)
and give it a verified email. `scripts/dev-trade.sh` does the setup for the trade app's dev
server.

:::danger[Dev fixture only]
`oms-dev-secret` is public. The OMS refuses to start with it unless the bind address is
loopback. Never run this fixture beyond your own machine.
:::
