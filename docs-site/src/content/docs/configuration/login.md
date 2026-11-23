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

## Groups

A group has members and, per portfolio, an access level:

| Level | Can |
|---|---|
| **view** | see all orders, positions and limits |
| **trade** | place, cancel and allocate own orders; see positions and limits |
| **manage** | everything, plus cancel any order, set limits, manage strategies |

Several groups give the highest level per portfolio. **Admin** is separate: a switch on
the Users page. The last active admin cannot be removed.

## Single sign-on

```toml
[auth]
public_base_url = "https://oms.example.com"

[auth.oidc]
issuer = "https://login.example.com/realms/company"
client_id = "openoms"
```

Put the secret in `OMS_OIDC_CLIENT_SECRET`, register the redirect URI
`{public_base_url}/auth/callback` with the provider, and restart. **People → Sign-in**
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
