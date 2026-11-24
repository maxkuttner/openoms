---
title: Sign-in and users
description: The first account is the admin; others sign up and wait for approval, or an admin adds them. Optional authenticator app and single sign-on.
---

Everyone signs in at `/app/`. The OMS has its own accounts (password, optional
authenticator app), so no identity provider is needed. Single sign-on (SSO) can be added
later. Each account is **admin**, **user** or **pending**.

## First start

On a fresh install the sign-in page offers **Create the admin account**. The admin can
manage the OMS but cannot trade until it joins a group with portfolio access.

For scripted installs, `oms auth bootstrap-admin --email you@example.com --name "You"`
prints a one-time password that must be changed at first sign-in and expires after 24
hours.

## Users

- **Sign-up:** new accounts are **pending** until an admin approves them on the **Users**
  page (and picks their groups) or rejects them.
- **Add user:** an admin creates the account directly with a password.
- From the Users page an admin also changes groups, grants admin, disables users
  (sessions and API keys stop at once), and manages API keys.
- Users manage their own API keys under **Trade → API**. A key acts as its user; the
  secret is shown once.

```toml
[auth]
enable_signup = true          # offer "Sign up"
default_user_role = "pending" # "user" activates sign-ups at once
```

## Passwords and MFA

- Passwords: 15–128 characters, not a common one.
- Authenticator app (TOTP), optional, under **Account → Two-step sign-in**, with ten
  one-time recovery codes.
- Too many failed attempts lock the account for 15 minutes.

`require_mfa = true` under `[auth.local]` makes the authenticator mandatory. Without it
passed, a session can only read: orders, strategies, limits and admin changes need MFA.
API keys and strategy tokens are exempt.

## Groups and grants

Two things decide what someone can do on a portfolio, and they need both:

- **Access** to the portfolio: *view*, *trade* or *manage*, set on the portfolio.
- The **grant** for the action, such as cancelling any order.

Without the grant the OMS refuses with `not granted: <key>`, and the cockpit hides the
button. **Admin** (a switch on the Users page) skips grant checks, but an admin still
needs portfolio access to trade. The last active admin cannot be removed.

### Grants

Open **Organisation → Groups → Default grants** to set what every user gets. A group adds
grants on top of that. A grant counts if it is on in the defaults or in any of the
person's groups. Nothing can take a grant away.

| Group | Grant | Controls |
|---|---|---|
| Trading | Place and cancel own orders | own orders |
| Trading | Cancel any order | other people's orders |
| Trading | Allocate block orders | allocation |
| Trading | Trade options and combos | option and combo orders |
| Strategies | Create strategies | new strategies |
| Strategies | Rotate and revoke strategy tokens | tokens, and archiving |
| Risk | See risk limits | reading limits |
| Risk | Set risk limits | changing limits |
| Account | Personal API keys | your own API keys |

Strategy tokens and the host CLI token are not grant-checked.

### Edit a group

Open a group on **Organisation → Groups**. Its tabs:

- **General:** name and description.
- **Users:** the members.
- **Portfolios:** the access level per portfolio. Several groups give the highest level.
- **Grants:** the grants this group adds.
- **Preview:** what its members end up with.

On an upgrade nothing changes. The defaults start with own orders, allocate, options,
create strategies, strategy tokens, see limits and personal API keys. Groups with
manage on a portfolio also get cancel any order and set limits. A newly reported
strategy's group gets the same two.

## Portfolios

Only admins create portfolios. Creating one also creates the group `portfolio <code>`
with the creating admin as its only member, at **view** access. It is an ordinary
group: raise it, add people, or delete it.

Open one under **Trading → Portfolios**. Its tabs:

- **Access:** who may use it. Add groups or single people, each at View, Trade or
  Manage. The highest level wins.
- **Venues:** where it may trade.
- **General:** name and details.

### Venues

A venue is a broker connection.

1. Open **Portfolios → Venues** on the portfolio.
2. Tick the venues it may trade on. IBKR also asks for the IBKR account.

- A portfolio with no venue cannot trade anywhere.
- The order ticket offers only the allowed venues.
- Removing a venue keeps history. Orders already placed keep booking fills.

## Single sign-on

```toml
[auth]
public_base_url = "https://oms.example.com"

[auth.oidc]
issuer = "https://login.example.com/realms/company"
client_id = "openoms"
```

Put the secret in `OMS_OIDC_CLIENT_SECRET`, register the redirect URI
`{public_base_url}/auth/callback` with the provider, and restart. **Security → Sign-in**
shows whether discovery works.

An SSO login is matched by issuer and subject. An unknown one gets in only if:

- `merge_accounts_by_email = true` and its **verified** email matches an account, or
- `[auth.oidc] enable_signup = true`, which creates a pending account.

:::caution[Merging by email trusts your provider completely]
Anyone your provider vouches for gets the matching account, admins included, without its
password or authenticator. Use it only with a provider you control (a company Entra ID
or Google Workspace), never a public one.
:::

Other options: `require_amr = ["mfa"]` to require MFA at the provider;
`required_claim` / `required_claim_value` to require a claim;
`[auth] policy = "sso_only"` to turn local sign-in off (only after an admin signs in via
SSO).

## Recover an admin from the host

```sh
oms auth recover-admin --email you@example.com
# Docker: docker exec <container> oms auth recover-admin --email you@example.com
```

Sets a new one-time password, removes the authenticator and signs the admin out
everywhere. It works only on the OMS machine (loopback, per-boot host token).

## HTTPS beyond localhost

Unless the server and `public_base_url` are both on loopback, `public_base_url` must be
`https://`, or the OMS refuses to start. Put TLS in front (nginx, Caddy,
`tailscale serve`).
