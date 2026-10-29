---
title: First run
description: Create the database and config with oms init, start the server, open the cockpit.
---

## 1. Have a Postgres 16

The OMS keeps everything — orders, positions, the instrument catalog, encrypted broker
credentials — in PostgreSQL 16. `oms init` needs a superuser login to it, once, to create
the database and the `oms` role the server then runs as.

No Postgres yet? Docker is the quickest way:

```sh
docker run -d --name oms-postgres --restart unless-stopped \
  -e POSTGRES_PASSWORD=postgres -p 127.0.0.1:5432:5432 \
  -v oms-pgdata:/var/lib/postgresql/data postgres:16
```

## 2. Initialise

```sh
oms init
```

It asks for the Postgres connection, with defaults in brackets:

```text
Postgres host      [localhost]
Postgres port      [5432]
Database name      [ods]
Superuser name     [postgres]
Superuser password:
```

Then it:

1. writes `~/.config/oms/oms.toml` (mode `0600`) with a generated master key, the `oms`
   role's password and a cockpit password;
2. creates the `oms` role and the database, applies every migration, and seeds reference
   data (currencies, ~2,800 venues, trading calendars);
3. prints the **cockpit login password** and where the config was written.

:::caution[Back up oms.toml]
The master key in `oms.toml` is the only thing that can decrypt the broker credentials you
store later. Lose it and they are gone. See
[Master key and backups](/openoms/operations/master-key-and-backups/).
:::

For scripts, `oms init --non-interactive` takes the same values from flags (`--host`,
`--port`, `--database`, `--username`, `--password`) and `POSTGRES_*` variables, and writes
the cockpit password to `oms.toml` instead of printing it.

`oms init` refuses to run if a config already exists in any
[location it searches](/openoms/configuration/file-locations/). To upgrade an existing
database, use `oms database migrate`.

## 3. Start the server

```sh
oms
```

It listens on `http://localhost:3001` and logs what it found. On a fresh install expect a
warning that the instrument catalog is empty — you fill it in the next guide.

## 4. Open the cockpit

Open `http://localhost:3001/cockpit/` and log in with the
cockpit password `oms init` printed. Forgot it? It is `admin_password` in
`~/.config/oms/oms.toml`:

```sh
grep admin_password ~/.config/oms/oms.toml
```

Next: [your first paper trade](/openoms/getting-started/first-paper-trade/).
