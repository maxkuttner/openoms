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
  [update](/openoms/operations/updating/), run `oms database migrate`.

```sh
oms database status
```

```text
server:   localhost:5432
database: ods (present)
role:     oms
migrations: 51 applied, 0 pending
```

## What's in it

One role, `oms`, owns everything. The `public` schema holds shared reference data —
instruments, venues, currencies. The `oms` schema holds the OMS's own tables — orders and
their events, portfolios, principals, accounts, positions, and encrypted credentials.
