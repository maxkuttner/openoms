---
title: Master key and backups
description: What to back up, why the master key matters, and how to rotate it.
---

## What to back up

| What | Why |
|---|---|
| `oms.toml` | Holds the **master key**, the `oms` role password and the cockpit password. |
| The `.env` beside it, if you have one | Secrets that are not in `oms.toml`, such as `OMS_OIDC_CLIENT_SECRET`, and on the test server the Authentik values (`OMS_AUTHENTIK_SECRET_KEY`, `OMS_AUTHENTIK_PG_PASSWORD`, `OMS_AUTHENTIK_BOOTSTRAP_PASSWORD`). |
| The database | Orders, executions, positions, portfolios, users and their roles, invites, sign-in identities, the audit log, and the encrypted broker and feed credentials. |

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
  credentials again.
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

Then restart the server so it uses the new key. If rotation fails partway, the
transaction rolls back and everything stays readable under the old key.

## Authentik is backed up separately

Sign-in accounts (passwords, Microsoft, Google and GitHub sources, the `email_verified`
attributes) live in Authentik's own Postgres, not in the OMS database. Back up its volume
or run `pg_dump` against it too, and keep `OMS_AUTHENTIK_SECRET_KEY`: Authentik cannot
read its stored secrets without it. The OMS database holds only the
`(issuer, subject)` of each sign-in, so a restored OMS pointed at a new, empty Authentik
needs fresh invites.
