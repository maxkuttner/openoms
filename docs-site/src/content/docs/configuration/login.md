---
title: Sign-in and users
description: Invite people, give them a role, and let them sign in to the /trade/ app through Authentik.
---

The admin cockpit (`/cockpit/`) uses one admin password. Everyone else signs in to the
separate trading app at `/trade/` through an OpenID Connect provider. The OMS is built
around [Authentik](https://goauthentik.io/), which brokers Microsoft, Google and GitHub
sign-in as well as email + password accounts. Any OIDC provider that reports a verified
email works the same way, but only Authentik is set up for you.

Access is by **invitation only**. You invite a person with an email address, a role and
a set of portfolios; they open the link you send them and sign in. Nobody else gets in,
not even read-only: an uninvited sign-in creates nothing and shows "Access not granted".

Sign-in is **off** by default: with no `[auth.oidc]` block, `/auth/*` and `/trade/`
return 404 and nothing else changes. You can still invite people before sign-in is
turned on; they just cannot use the link yet.

## Set up Authentik

Authentik is a separate service. The repository's compose files run it for you:

- locally, `docker compose --profile oidc up -d` (admin UI at `http://localhost:9000/if/admin/`);
- on the [test server](#on-the-test-server), as part of `deploy/docker-compose.yml`.

Both load the blueprint `docker/authentik/blueprints/oms-dev.yaml`, which creates the
`oms` OAuth2/OpenID provider and application, with the issuer
`http://localhost:9000/application/o/oms/` and client ID `oms`.

:::danger[The blueprint's client secret is public]
The blueprint sets the client secret to `oms-dev-secret`, which is in the repository.
Before anything real runs on it, open the `oms` provider in Authentik, set a new
secret, and put the same value in `OMS_OIDC_CLIENT_SECRET`. The same goes for the
Authentik admin (`akadmin`) password and secret key: the defaults in the root compose
file are for local use only.
:::

To set it up by hand instead, create an OAuth2/OpenID provider (confidential client)
with the redirect URI `{public_base_url}/auth/callback`, e.g.
`https://oms.example.com/auth/callback`, and an application that uses it.

The cockpit's **Sign-in** page shows the configured issuer, whether discovery works (and
the reason if not), and the exact redirect URL to paste into Authentik.

## Point the OMS at it

1. Add the block to `oms.toml`:

   ```toml
   [auth.oidc]
   issuer = "http://localhost:9000/application/o/oms/"
   client_id = "oms"
   public_base_url = "http://localhost:3001"
   ```

2. Put the client secret in the environment, never in `oms.toml`:

   ```sh
   OMS_OIDC_CLIENT_SECRET=...
   ```

   A `.env` next to `oms.toml` works (see
   [Config file locations](/configuration/file-locations/#env-files)).

3. Restart the OMS.

If the provider is misconfigured or unreachable, sign-in stays off for that run and the
error is logged; order routing and the cockpit are unaffected.

## Sign-in methods

Authentik owns the "who is this" question. Add whichever methods you want:

- **Microsoft, Google, GitHub.** In Authentik, add each as a *source* (Directory →
  Federation and Social login). Each needs an app registration at that provider, with
  the callback URL Authentik shows you.
- **Email + password.** Create the account in Authentik by hand. No enrollment flow
  ships with the OMS blueprint; if you build one, it must use an **invitation stage with a
  fixed email**, so the address on the account is the one that was invited.

### The verified-email rule

An invite is claimed by email, and only by a **verified** one. The OMS refuses a sign-in
unless the ID token says `email_verified: true` and the address equals the invited one
(compared case-insensitively). If it differs, the person sees "This invite is for
j…@fundco.com. Sign in with that address, or ask for a new invite." and the invite stays
pending.

The `oms` provider reports `email_verified` from one place only: the Authentik user
attribute `email_verified`, true only when it is a true boolean. Authentik's stock
`email` scope claims every address is verified, so the blueprint replaces it. You must
make the attribute true correctly:

| Sign-in method | How `email_verified` becomes true |
|---|---|
| Google | Source property mapping sets the attribute from the provider's `email_verified` claim. |
| Microsoft | Same: set it from the provider's verified-email claim, and leave it unset otherwise. |
| GitHub | Set it only from the account's primary verified email (GitHub's `/user/emails` list), never from the profile address. |
| Email + password (hand-made) | An admin sets the attribute `email_verified: true` (a boolean) on the user, after confirming the address. |
| Email + password (enrollment flow) | The invitation stage fixes the email to the invited address; set the attribute in that flow. |

A missing or false attribute is refused with the reason shown. Never set it for sources
that do not prove ownership of the address.

After the first sign-in, the login is bound to the provider's `(issuer, subject)`, not to
the email. Every later sign-in looks up that pair only. Two logins are never merged
because their emails match; to add a second provider for someone, send a fresh invite.

## Roles and permissions

Each user has one role. The backend checks every request against the role's permissions,
inside the portfolios the user has been given. The grant flags decide where a role
applies: *view* puts a portfolio in scope, *trade* allows creating and cancelling there.

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

"Own" means the order was submitted by that user. A fourth role, **strategy**, is
for automated [strategies](/strategies/fix/) and cannot be assigned to a person. The
admin console stays separate (the admin password) and is not part of these roles.

## Invite someone

On the cockpit's **Users** page choose **Invite user**, then fill in the email, role and
portfolios (view, and optionally trade, for each). The dialog shows the invite link and
its expiry (7 days). The OMS sends no email: copy the link and send it yourself.

The link is `{public_base_url}/trade/invite#{token}`. It opens the invite page, which
shows the role and portfolios and offers the enabled sign-in methods ("Set a password"
and/or "Sign in with SSO"); the person then lands in the trade app.

The new user appears as **Invited** and cannot sign in or use API keys until they accept.
From the list you can also:

- **Copy link**, **Revoke** or **Reissue** a pending invite. A revoked or expired invite
  shows its own message ("This invite expired. Ask for a new one.").
- Change the **role** or **portfolios**; it applies on the next request.
- **Disable** a user: sessions are deleted at once and their API keys stop working.
  **Enable** reverses it.
- Create and revoke the user's **API keys**. A key belongs to the user; a strategy's
  tokens belong to the strategy and are made on the **API** page.

The home page has a **setup checklist** until you are done: connect a venue, create a
portfolio, turn on sign-in, invite a user, and see a first order in the blotter. Invites,
acceptances, role and portfolio changes and revocations are recorded with who did them.

### What a person sees when refused

The trade app shows a plain page: "Access not granted" (no invite), "This invite expired",
the invite was revoked, "This invite is for a different email", or a message that the
provider did not report a verified email.

## Strategies and portfolio managers

Every strategy has a **responsible user**, an active person, shown on the **Strategies**
page. Handing a strategy to someone else is an explicit, audited action (**Handover**).
If the responsible user is disabled the strategy is flagged "needs a new responsible user"
and cannot be started until you pick another.

A PM can create strategies, hand them over, and set limits in the portfolios they manage,
through the trade app's endpoints `GET`/`POST /strategies`,
`POST /strategies/{code}/handover`, `GET /portfolios/{id}/limits` and
`PUT /portfolios/{id}/limits/{instrument_id}`. Traders and viewers see limits read-only.

## Only some users

Invites already gate access, so this is rarely needed. To additionally require a claim in
the ID token:

```toml
required_claim = "groups"
required_claim_value = "oms-traders"
```

Both keys are needed for the check to apply.

## Sessions

A session ends after `idle_ttl_minutes` without activity (default 30) and after
`absolute_ttl_hours` regardless (default 12).

## HTTPS is required beyond localhost

The session cookie must be marked `Secure`. So unless both the bind address and
`public_base_url` are loopback, `public_base_url` **must** start with `https://`; the
OMS refuses to start otherwise. Authentik's own address, used as the issuer, must be
https too unless it is loopback.

The usual setup is a loopback bind with TLS terminated in front: a reverse proxy (nginx,
Caddy) or `tailscale serve`, with `public_base_url` set to the HTTPS address users open.

## Trying it locally

```sh
docker compose --profile oidc up -d
```

The first start takes a minute or two while Authentik migrates its database. Use the
`[auth.oidc]` block from above with `OMS_OIDC_CLIENT_SECRET=oms-dev-secret`, then in the
Authentik admin UI (`akadmin` / `admin`, dev only) create a user with your email and set
its attribute `email_verified: true`. Invite the same email from the cockpit's Users
page, open the link, and sign in as that user. `scripts/dev-trade.sh` does the whole
setup for the trade app's dev server.

## On the test server

`deploy/docker-compose.yml` runs Authentik next to the OMS. Three values are **required**
in `/opt/oms/config/.env` (`deploy.sh` passes that file to compose, and the stack does not
start without them):

```sh
OMS_AUTHENTIK_SECRET_KEY=...
OMS_AUTHENTIK_PG_PASSWORD=...
OMS_AUTHENTIK_BOOTSTRAP_PASSWORD=...
```

Put the server behind HTTPS (Tailscale Serve), make the issuer that https address, and
change the blueprint's `oms-dev-secret` as warned above.

## Upgrading from the earlier login

Existing OIDC users keep their login, bound to the issuer in your `[auth.oidc]` block.
Switching to a different identity provider (for example from the old local test provider to Authentik) changes the issuer, so those users no longer match:
send each a fresh invite (**Sign-in link** on the Users page re-links an existing user
through a new invite). People who existed as logins before get the PM role; strategy
users keep the strategy role. Unused service and desk entries are removed; used ones
become key-only users that you can disable from Users.
