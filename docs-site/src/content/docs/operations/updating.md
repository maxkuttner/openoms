---
title: Updating
description: Move to a new release with oms update, migrate, restart — or roll back.
---

## Update

```sh
oms update
```

:::note[On v0.2.0?]
`oms update` arrived after v0.2.0. On that version, update once by re-running the
installer (`curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh`); from then
on, `oms update` works.
:::

It looks up the latest release, downloads the binary for your platform with the
release's `SHA256SUMS`, and refuses on a checksum mismatch. It then runs the new binary to
check it reports the expected version, and only then swaps it in place of the running
one. The swap is a rename in the same directory, so an interrupted update leaves the old
binary untouched.

```text
updating oms 0.2.0 → 0.3.0 (x86_64-unknown-linux-gnu)
checksum ok; replaced /home/you/.local/bin/oms
```

Then finish the update yourself — `oms update` deliberately does neither step:

1. **Apply any new migrations.** The server never changes its database on its own;
   this command is the only thing that does:

   ```sh
   oms database migrate
   ```

   It connects as the Postgres superuser. If `oms init` started PostgreSQL in Docker
   for you, that password was generated and lives in the container:

   ```sh
   POSTGRES_PASSWORD="$(docker exec oms-postgres printenv POSTGRES_PASSWORD)" oms database migrate
   ```

2. **Restart the server**, so it runs the new binary. A running server keeps executing
   the old one until then.

## Check without installing

```sh
oms update --check
```

Prints whether a newer release exists, and changes nothing.

## A specific version, or a rollback

```sh
oms update --version v0.2.0
```

Installs exactly that release — older ones included. Note that migrations only go
forward: rolling the binary back does not undo `oms database migrate`.

## Built from source?

`oms update` refuses to replace a binary built from a checkout (one under a cargo
`target/` directory). Update it the way you built it:

```sh
git pull
(cd cockpit && npm ci && npm run build)
cargo build --release
```

## Reinstalling instead

The installer works for updates too, and always installs the latest release:

```sh
curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
```
