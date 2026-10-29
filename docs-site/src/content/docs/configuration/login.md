---
title: Login (OIDC)
description: Let traders sign in to the /trade/ app through your identity provider.
---

The admin cockpit (`/cockpit/`) uses one password. Traders sign in to the separate
trading app at `/trade/` through an OpenID Connect provider — Keycloak, Okta, Auth0,
Entra ID, Google and so on. A signed-in trader sees only the portfolios their principal
is granted.

Login is **off** by default: with no `[auth.oidc]` block, `/auth/*` and `/trade/` return
404 and nothing else changes.

## Enable it

1. At your provider, register a **confidential client** with the redirect URI
   `{public_base_url}/auth/callback`, e.g. `https://oms.example.com/auth/callback`.
2. Add the block to `oms.toml`:

   ```toml
   [auth.oidc]
   issuer = "https://idp.example.com/realms/oms"
   client_id = "oms"
   public_base_url = "https://oms.example.com"
   ```

3. Put the client secret in the environment — never in `oms.toml`:

   ```sh
   OMS_OIDC_CLIENT_SECRET=...
   ```

   A `.env` next to `oms.toml` works (see
   [Config file locations](/openoms/configuration/file-locations/#env-files)).

4. Restart the OMS.

## Who can trade after signing in

The first time someone signs in, the OMS creates a principal for them automatically,
keyed by the provider's subject — and with **no grants**. They can sign in but see and
trade nothing until an admin grants that principal a portfolio (cockpit → Principals →
Grants, or `POST /admin/principals/{id}/grants`). An existing service or strategy
principal, or a disabled one, can never be taken over by a login.

If the provider is misconfigured or unreachable, login stays off for that run and the
error is logged; order routing and the cockpit are unaffected.

## Only some users

To admit only members of a group, require a claim in the ID token:

```toml
required_claim = "groups"
required_claim_value = "oms-traders"
```

Both keys are needed for the check to apply.

## Sessions

A trader's session ends after `idle_ttl_minutes` without activity (default 30) and
after `absolute_ttl_hours` regardless (default 12).

## HTTPS is required beyond localhost

The session cookie must be marked `Secure`. So unless both the bind address and
`public_base_url` are loopback, `public_base_url` **must** start with `https://` — the
OMS refuses to start otherwise.

The usual setup is a loopback bind with TLS terminated in front: a reverse proxy (nginx,
Caddy) or `tailscale serve`, with `public_base_url` set to the HTTPS address users open.

## Trying it locally

A clone of the repository includes a disposable Keycloak with a ready-made realm. It is
for testing only:

```sh
docker compose --profile oidc up -d keycloak
```

```toml
[auth.oidc]
issuer = "http://localhost:8090/realms/oms-dev"
client_id = "oms"
public_base_url = "http://localhost:3001"
```

```sh
OMS_OIDC_CLIENT_SECRET=oms-dev-secret
```

Sign in at `http://localhost:3001/trade/` as `trader` / `trader`.
