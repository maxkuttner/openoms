# Container setup from environment variables — design

Status: design, owner-approved approach (b): the image's start script sets the database up; the `oms` server stays hands-off.

## Intent

openoms should deploy like other self-hosted apps (Gitea, Immich, Plausible): one compose file, settings as environment variables, `docker compose up -d`, done. Today the published image needs a one-off `docker compose run --rm oms init` before `up`, and the public URL can only be set by editing `oms.toml` inside the config volume. Platforms such as Dokploy, Coolify and Portainer deploy from a compose file alone, so they cannot do either step.

## Goals

- The published image starts from a compose file with environment variables only: no `oms init`, no hand-edited `oms.toml`.
- On first start it creates the database; on every start it applies pending migrations.
- The public URL is settable by environment variable.
- `docs-site/public/compose.yaml` and the installation guide become "set the env vars, `docker compose up -d`".

## Non-goals

- Changing what `oms` (the server) does on start. It still never creates or migrates a database (decision of PR #24 stands).
- Changing `oms init` or `oms database init` behaviour on a laptop or a plain host install.
- Secrets management beyond environment variables (Docker secrets, `_FILE` variants) — possible later.

## Anti-goals

- A container that silently starts against an uninitialised or half-migrated database.
- A container that starts without a master key and quietly can't decrypt stored broker credentials.
- Breaking existing compose installs that already have an `oms.toml` in the `oms-config` volume.

## Approaches

- **(a) Server self-provisions on start** (like PR #20). Rejected by the owner: it reverses #24's rule that starting the server never changes a database.
- **(b) The image's start script runs the database setup, then starts the server (chosen).** Containers deploy with no manual step; the binary's behaviour is unchanged everywhere else.

## Design

### 1. Settings from environment

Every setting `oms init` writes already has an environment variable, read with precedence flag > env > `oms.toml`:

| Setting | Variable | Status |
|---|---|---|
| Database host, port, superuser, database name | `POSTGRES_HOST`, `POSTGRES_PORT`, `POSTGRES_USERNAME`, `POSTGRES_DATABASE` | exists |
| Superuser password (setup only) | `POSTGRES_PASSWORD` | exists |
| The OMS's own role password | `OMS_PASSWORD` | exists |
| Master key | `OMS_MASTER_KEY` | exists |
| Listen address | `OMS_BIND_ADDR` | exists (image sets `0.0.0.0:3001`) |
| OIDC client secret | `OMS_OIDC_CLIENT_SECRET` | exists |
| **Public URL** (`[auth].public_base_url`) | **`OMS_PUBLIC_BASE_URL`** | **new** |

`OMS_PUBLIC_BASE_URL` overrides `[auth].public_base_url` (and the deprecated `[auth.oidc].public_base_url`) when set and non-empty. It feeds the CSRF origin rule, cookie policy and OIDC redirect URI exactly as the file value does today.

The server already runs with no `oms.toml` when these variables are set. `oms init` stays the guided path for host installs; containers no longer need it.

### 2. `oms database setup` (new CLI command)

One idempotent verb for scripts:

- database absent → same as `oms database init` (roles, database, schema, access, seed);
- database present → same as `oms database migrate`;
- any failure (unreachable server, wrong password, partial state that `init` refuses) → non-zero exit with the existing error text.

It needs the superuser credentials (`POSTGRES_*`) and the role password (`OMS_PASSWORD`). `init` and `migrate` stay unchanged and strict; `setup` only chooses between them, so their safety rules (refuse the shipped default password on a non-loopback host, refuse a half-made install) still apply.

Name is the builder's call if `setup` clashes with an existing verb; it must be a `database` subcommand.

### 3. The image's start script

`ENTRYPOINT` becomes a small POSIX `sh` script (in the repo, generic, no deployment specifics):

- no arguments (the default `oms` server start):
  1. refuse to start with a clear message unless `OMS_MASTER_KEY` is set or the config file provides `[oms].master_key`;
  2. if `POSTGRES_PASSWORD` is set: run `oms database setup`, exit on failure;
     if it is not set: skip setup and say so (an operator managing the database by hand);
  3. `exec oms`.
- any arguments: `exec oms "$@"` unchanged, so `docker compose run --rm oms init`, `oms database status` etc. keep working.

Existing installs: an `oms.toml` in `/config` keeps working — its values apply where no environment variable overrides them, and `setup` on an initialised database just migrates (which today's users run by hand after `pull`).

### 4. Compose file and docs

`docs-site/public/compose.yaml`:

- `oms` gets `OMS_PASSWORD`, `OMS_MASTER_KEY`, `OMS_PUBLIC_BASE_URL` from `.env`, alongside `POSTGRES_PASSWORD`;
- the header's setup becomes: generate the three secrets into `.env` (one `openssl rand` line each), `docker compose up -d`, open the URL;
- the `oms-config` volume stays (backward compatibility, and where `oms init` writes if someone still uses it).

Installation guide: the Docker section follows the compose header; a short "behind a domain or reverse proxy" note says to set `OMS_PUBLIC_BASE_URL`. The `oms init` route stays documented for host installs. Warn prominently: back up `OMS_MASTER_KEY`; it is the only thing that decrypts stored broker credentials.

### 5. Tests

- Config: `OMS_PUBLIC_BASE_URL` overrides the file value; empty is ignored.
- `oms database setup`: absent database → initialised; present → migrated; second run is a no-op; wrong superuser password → non-zero exit (DB tests run against a throwaway Postgres, as today).
- Start script: CI's image job starts the image with env only against a fresh Postgres and gets `/health` 200, then restarts it and gets 200 again (idempotent setup); without `OMS_MASTER_KEY` it exits non-zero with the message.
- Docs-site: the compose file still parses; its header and the guide name the same variables.

## Left to the builder

- Script location and name (e.g. `docker/entrypoint.sh`), copied into the image.
- Whether `setup` reuses `init`/`migrate` functions directly or calls them via the CLI layer.
- Exact wording of messages.
