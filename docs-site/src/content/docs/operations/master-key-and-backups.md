---
title: Master key and backups
description: What to back up, why the master key matters, and how to rotate it.
---

## What to back up

| What | Why |
|---|---|
| `oms.toml` | Holds the **master key** and the `oms` role password (and the deprecated shared admin password, if still set). |
| The `.env` beside it, if you have one | Secrets that are not in `oms.toml`, such as `OMS_OIDC_CLIENT_SECRET`. |
| The database | Orders, executions, positions, portfolios, users and their roles, invites, password hashes, sealed authenticator secrets, SSO identities, the audit log, and the encrypted broker and feed credentials. |

The `host-token` file next to `oms.toml` is not a backup item: the server writes a new one at every start.

Back up the database with standard Postgres tools, for example:

```sh
pg_dump -h localhost -U postgres -Fc ods > oms-$(date +%F).dump
```

Keep `oms.toml` somewhere safe and separate from the database dumps: a dump is only
readable credential-wise together with the key, and anyone holding both holds your broker
credentials.

## The master key

Broker and feed credentials are stored in Postgres encrypted with the master key — the
`master_key` line in `oms.toml` (or `OMS_MASTER_KEY`). The OMS never stores the key in the
database.

- **Lose it** and every stored credential is unreadable. There is no recovery: enter the
  credentials again. Authenticator (TOTP) secrets are sealed with the same key, so every
  user with a local account has to enrol their authenticator again. Recover each admin from
  the host with `oms auth recover-admin`; for everyone else an admin sends a new sign-in link.
- **Point two installs at one database** and they must share the same key, or each will
  fail to decrypt what the other wrote.
- **A wrong key** is caught at startup: each credential that fails to decrypt is logged,
  and if credentials exist but none decrypt, the server refuses to start rather than run
  as if none were configured.

## Rotating the master key

```sh
oms config rotate-key
```

This re-encrypts every stored credential under a newly generated key, inside one
database transaction, and prints the new key:

```text
rotated 2 credential(s) to a new master key.

new master key: base64:…
```

:::danger[Save the new key immediately]
When the command returns, the database is already encrypted under the **new** key.
The printed key is its only copy. Write it into `oms.toml` as `master_key` before doing
anything else, and keep the old key until that edit is saved.
:::

:::caution[Authenticator secrets are not re-wrapped]
`rotate-key` re-encrypts broker and feed credentials only. Authenticator (TOTP) secrets stay
sealed under the old key, so after a rotation local-account users must enrol again, exactly
as if the key had been lost. Rotate only when you accept that.
:::

Then restart the server so it uses the new key. If rotation fails partway, the
transaction rolls back and everything stays readable under the old key.

## Your identity provider is backed up separately

If you use SSO, accounts live at your identity provider, not in the OMS database, which
holds only the `(issuer, subject)` of each sign-in. Back the provider up the way you
already do. A restored OMS pointed at a different provider needs fresh sign-in links.
