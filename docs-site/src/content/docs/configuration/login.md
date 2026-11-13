---
title: Sign-in and users
description: Local accounts with an authenticator app by default, invites for everyone else, and optional single sign-on with your own identity provider.
---

Everyone signs in the same way, to the cockpit (`/cockpit/`) and to the trading app
(`/trade/`): one session, one account per person. The OMS has its own accounts (a password
plus an authenticator app), so a fresh install needs no identity provider, mail server or
extra container. If your organisation has single sign-on (SSO), you can add it later, next
to local accounts or instead of them.

Access is by **invitation only**. There is no public sign-up, and a successful sign-in at an
identity provider creates nothing: an uninvited sign-in shows "Access not granted".

## First admin

`oms init` asks for the first admin's email and name, creates that admin, and, when you run
it at a terminal, prints a one-time link:

```text
First admin (you@example.com): open this one-time link to choose a password and set up TOTP:
    http://localhost:3001/trade/invite#…
```

Open it, choose a password, scan the QR code with an authenticator app, and save the ten
recovery codes it shows (once). With `--non-interactive` (CI, Ansible) the link is not
printed, since it would end up in a log; the output names the command that mints one later
at a terminal.

If you skipped it, or lost the link, use the host CLI:

```sh
oms auth bootstrap-admin --email you@example.com --name "Your Name"
```

This works only while no active admin exists. The admin chooses their own password through
the link; nobody sets one for them. Admin is a separate capability from trading: an admin
can run the console but cannot place orders until someone gives them a role and portfolios
(see [Roles](#roles-and-permissions)).

## Local accounts

- **Password.** 15 to 128 characters, no composition rules. Stored as an Argon2id hash.
- **Authenticator app (TOTP).** Standard 6-digit codes, renewed every 30 seconds. The
  secret is sealed with the [master key](/operations/master-key-and-backups/).
- **Recovery codes.** Ten single-use codes, shown once at enrolment, each standing in for
  one authenticator code.
- **Rate limits.** More than 5 failed attempts per account, or 20 per client address, in
  15 minutes are refused, and 10 failures in a row lock the account for 15 minutes. Every
  failure looks the same, so a response never reveals whether an email has an account.

### Where MFA is required

A session that signed in with a password but has not passed the authenticator check can
read, and nothing else. These need a session that passed MFA:

- submitting, cancelling or allocating orders;
- creating or handing over strategies, and changing limits;
- every change in the admin console (users, roles, connections, keys).

Reads never need it, and neither does adding or removing instruments on your own
watchlist: that is a personal view setting, not an order or admin action. API keys, strategy tokens and the host token are exempt: they are
machine credentials with their own scope. For SSO sessions see
[`require_amr`](#require-mfa-from-the-identity-provider).

## Invite someone

On the cockpit's **Users** page choose **Invite user**, then enter the email, a role and
the portfolios (view, and optionally trade, for each). The dialog shows the invite link and
its expiry (7 days). The OMS sends no email: copy the link and send it through a channel you
trust, since whoever holds it can start the claim.

The link is `{public_base_url}/trade/invite#{token}`. It opens a page that shows the role
and portfolios first, then offers the enabled sign-in methods: set a password and
authenticator, or sign in with SSO. The new user appears as **Invited** and cannot sign in
or use API keys until they accept.

**Sign-in method** in the dialog restricts how the invite can be claimed: **Any sign-in method** (the
default), **Email + password only**, or **SSO only**. The API takes the same choice as
`"method": "any" | "local" | "oidc"` on `POST /admin/users/invite` and on reissue; any
other value is refused with 422.

From the list you can also **copy**, **revoke** or **reissue** a pending invite (a revoked
or expired link shows its own message), change the **role** or **portfolios** (effective on
the next request), **disable** a user (sessions end at once and API keys stop), and create
or revoke their **API keys**.

Revoking access ends the user's open live-update connections immediately. Revocations the
running server is not told about, a session reaching its absolute lifetime or a host-CLI
`oms auth recover-admin`, close those connections at the next check, within 15 seconds.

The home page has a **setup checklist** until you are done: connect a venue, create a
portfolio, confirm sign-in works, invite a user and see a first order. Invites,
acceptances, role and portfolio changes, enrolments and recoveries are recorded with who
did them.

## Roles and permissions

Each user has one role. The backend checks every request against the role's permissions,
inside the portfolios the user has been given: *view* puts a portfolio in scope, *trade*
allows creating and cancelling there.

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
[strategies](/strategies/fix/) and cannot be given to a person. **Admin** is not a role: it
is a switch on the Users page that opens the admin console. The last active admin cannot be
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
| `[auth.session] idle_ttl_minutes` | `30` | A session ends after this much inactivity. |
| `[auth.session] absolute_ttl_hours` | `12` | A session ends after this regardless. |
| `[auth.oidc] issuer`, `client_id` | required | |
| `[auth.oidc] scopes` | `openid profile email` | |
| `[auth.oidc] required_claim`, `required_claim_value` | none | Also require this claim in the ID token (both keys, e.g. `groups` and `oms-traders`). Invites already gate access, so this is rarely needed. |
| `[auth.oidc] trust_email_verified` | `true` | See below. |
| `[auth.oidc] require_amr` | `[]` | See below. |

### How an SSO invite is claimed

The invitee opens the invite link, sees the role and portfolios, and chooses SSO. What
happens next depends on what the provider says about the email:

- **Verified email.** If the ID token says `email_verified: true` and the address equals the
  invited one (case-insensitive), the invite is claimed. A different address is refused and
  the invite stays pending.
- **No verified email, or `trust_email_verified = false`.** The person signs in at the
  provider but gets **no access** yet. The sign-in appears under **Waiting for approval** on
  the Users page, with who it was. Check by a channel you trust that it is the right person,
  then **Approve** or **Reject**. Approval binds that exact sign-in to the invited user.
- **Pre-bind.** If you already know the provider's stable user ID (`sub`), an admin can
  attach it to the invited user directly (`POST /admin/users/{id}/identities`), with no
  pending step.

After the first sign-in the login is bound to the provider's `(issuer, subject)`, never to
the email: later sign-ins look up that pair only, and two logins are never merged because
their emails match. To let someone use a second sign-in method, use **Sign-in link** on
their row; the person must open it in a session that passed MFA, or an admin approves the
pending link.

### Providers

| Provider | Notes |
|---|---|
| Google | Sends `email_verified`. |
| Okta, Keycloak, Authentik | Send `email_verified` when the user's email is verified in the provider. Check that yours does, or rely on approval. |
| Microsoft Entra ID | Does not reliably send `email_verified`, and its `email` or `upn` is not proof of a mailbox, so sign-ins arrive as pending approvals. Use your tenant's issuer (`https://login.microsoftonline.com/{tenant-id}/v2.0`), never `common`. |

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

Local sign-in and enrolment are refused and the login page shows only SSO, whatever
`[auth.local].enabled` says (`[auth.local] enabled = false` has the same effect). Set this
only after SSO works and an admin has signed in through it.

The host CLI still works, but the links `oms auth bootstrap-admin` and
`oms auth recover-admin` print enrol a **local** password and authenticator, which this
policy refuses; the command prints a note saying so. To use such a link, set
`policy = "local_and_sso"` and restart, enrol with the link, then set `sso_only` back and
restart.

## Recover an admin from the host

If nobody can sign in as an admin (authenticator and recovery codes lost, or a provider
outage), recover from the machine the OMS runs on:

```sh
oms auth recover-admin --email you@example.com
```

This resets the admin's password and authenticator, signs them out everywhere, and prints a
new one-time enrolment link. It is audited. To create a new admin while one already exists,
add `--force-recovery` to `oms auth bootstrap-admin`.

These commands use a per-boot **host token**, the file `host-token` next to `oms.toml`
(mode 0600, replaced at every start). The server accepts it from loopback addresses only,
so it is no remote door. Under Docker with bridge networking, run them in the container:

```sh
docker exec <container> oms auth recover-admin --email you@example.com
```

## Upgrading from the shared admin password

Earlier versions had one cockpit password (`[server].admin_password` or
`OMS_ADMIN_PASSWORD`). After upgrading, it still opens the cockpit, but **read-only**, plus
one action: **create the first admin**. Do that (or run `oms auth bootstrap-admin`), sign in
as the new admin and finish enrolment. As soon as a named admin signs in with MFA, the
shared password is switched off for good and you can delete it from the config. The
loopback default `openoms-dev` and `OMS_ADMIN_AUTH_ENABLED=false` no longer exist. If
something goes wrong in between, host recovery above is the way back in.

Existing SSO users keep their login as long as the `issuer` is unchanged. If you moved to a
different provider, send each person a **Sign-in link**.

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
