---
title: Config file locations
description: Where oms looks for oms.toml and .env, and where oms init writes them.
---

`oms` finds its config the same way from any directory — a shell, systemd, cron or
`sudo` — so you never have to `cd` somewhere first.

## Search order

The first file that exists wins:

1. **`$OMS_CONFIG`**, if set — used even if the file does not exist yet.
2. **`./oms.toml`** in the current directory.
3. **`~/.config/oms/oms.toml`** — or `$XDG_CONFIG_HOME/oms/oms.toml` when
   `XDG_CONFIG_HOME` is set.
4. **`/etc/oms/oms.toml`** — for a machine-wide install.

With none of them present, the OMS runs on built-in defaults. When it then cannot reach
its database, the error lists every location it searched.

## Where `oms init` writes

`oms init` writes to `$OMS_CONFIG` if set, otherwise to the per-user location,
`~/.config/oms/oms.toml`, creating the directory with mode `0700` and the file with mode
`0600`.

It never writes `/etc/oms/oms.toml` — that needs root. For a machine-wide install, run
`oms init` as usual and move the file there yourself:

```sh
sudo mkdir -p /etc/oms
sudo mv ~/.config/oms/oms.toml /etc/oms/oms.toml
```

Make sure the user the OMS runs as can read it, and nobody else can.

`oms init` refuses to run when a config already exists in **any** of the searched
locations, so you can never end up with two files holding different master keys.

## `.env` files

Environment variables can be kept in a `.env` file (`KEY=value` lines). Two are loaded:

1. `./.env` in the current directory;
2. the `.env` next to the config file found above — e.g. `~/.config/oms/.env`.

Neither overrides a variable already set in the real environment, and `./.env` wins over
the one beside the config.

Use it for settings with no `oms.toml` key, such as `OMS_OIDC_CLIENT_SECRET` or
`OMS_SYNC_ON_BOOT` — see [the reference](/openoms/configuration/reference/#other-environment-variables).
