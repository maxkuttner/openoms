---
title: Database commands
description: Create, migrate, inspect and drop the OMS database.
---

```sh
oms database status           # what exists, and which migrations are pending
oms database migrate          # apply pending migrations (safe to re-run)
oms database init             # create the role, database, schema and reference data
oms database init --resume    # finish an init that failed partway through
oms database drop             # destroy the database (the oms role is kept)
```

`oms init` already runs `oms database init` for you, after writing `oms.toml`. Use these
directly to upgrade, inspect, or start over.

- `init`, `migrate` and `drop` connect as the Postgres **superuser** — they prompt for its
  password, or take `--password` / `POSTGRES_PASSWORD`. `status` connects as the `oms`
  role.
- `init` refuses if the role or the database already exists. `--resume` creates only
  what is missing and re-runs the remaining steps, all of which are safe to repeat.
- `drop` against a database that is not on `localhost` needs `--yes`.
- The server itself never creates or migrates anything: after an
  [update](/operations/updating/), run `oms database migrate`.

```sh
oms database status
```

```text
server:   localhost:5432
database: ods (present)
role:     oms
migrations: 61 applied, 0 pending
```

## What's in it

One role, `oms`, owns everything. The `public` schema holds shared reference data —
instruments, venues, currencies. The `oms` schema holds the OMS's own tables — orders and
their events, portfolios, users, strategies, roles and invites, accounts, positions, and encrypted credentials.

Users and access are a small cluster in the `oms` schema. Every person and every strategy
has one row in the identity record that orders and audit entries point at:

```text
role ──< role_permission                what each role may do
  │
  └──< role assignment >── user / strategy ──< sign-in identity    (issuer, subject)
                              │   │
                              │   ├──< local_credential           password hash, sealed TOTP secret, lockout
                              │   │      └──< recovery_code       ten single-use codes, stored hashed
                              │   ├──< principal_admin            the admin capability, separate from roles
                              │   ├──< user_session               auth_method, MFA-verified time
                              │   ├──< invite                     one link per invited email, token stored hashed
                              │   │      └──< pending_identity_binding   SSO sign-in waiting for admin approval
                              │   ├──< portfolio grant            view / trade / allocate per portfolio
                              │   ├──< api_key
                              │   └──< audit_event                invites, role and grant changes, revocations
                              │
                        strategy (own record + a responsible user, who is a person)

system_setting                          key/value switches, e.g. whether the shared admin password still works
```
