# Guided `oms init` and one Getting Started page — design

Date: 2026-10-29
Status: draft, awaiting approval

## Why

A trader with a server and a terminal should go from nothing to a logged-in cockpit
without reading three pages or typing a `docker run` line. Today:

- Installation and First run are two pages; the OS-specific bits (PATH on macOS, OpenSSL
  3 on Linux) are mixed into one flow everyone has to read.
- "Read the script" links to `/openoms/install.sh`, which downloads the file instead of
  showing it.
- "No Postgres yet?" hands the user a `docker run` command to copy. The choice between
  bringing a database and having one made for you is real, but it belongs in setup, not
  in prose.
- `oms init` asks only for Postgres. The cockpit password is always generated; the bind
  address is always `localhost:3001` and changed by editing `oms.toml`.
- The installer's closing hint still says `oms database init` (stale: `oms init` is the
  first-run command) and points at `docker compose` "from a clone of the repo".

## Goals

1. `oms init` asks every first-run decision, each with a default Enter accepts:
   database source, Postgres connection, cockpit admin password, listen address.
2. Choosing "start one in Docker" makes `oms init` run Postgres 16 itself and connect to
   it — no command for the user to copy.
3. The installer stays prompt-free except one closing question: run `oms init` now?
4. One **Getting started** page replaces Installation + First run, with OS tabs.
5. `--non-interactive` can express every choice the prompts can.

## Non-goals

- Broker credentials in `oms init`. The cockpit is the path (Broker connections →
  Credentials → Load universe prompt); asking twice would split the flow.
- A published OMS container image (GHCR). Separate, later project. `deploy/` keeps
  building from source for the maintainer's server.
- Windows, or new prebuilt targets.
- systemd/launchd service install.
- Changing the admin password after init (it stays an `oms.toml` edit for now).

## Anti-goals

- A prompt that blocks `curl … | sh` or a scripted `oms init --non-interactive`.
- A secret printed when the user typed it themselves, or printed in non-interactive
  mode (unchanged rule: secrets never land in captured logs).
- Persisting the Postgres superuser password (unchanged rule from `build_file_config`).
- A half-done install with no stated way out: every failure after `oms.toml` is
  written names the recovery command, as today.

## The `oms init` flow

Prompts use **`dialoguer`** (console-rs): `Select` for the database menu, `Input` with
defaults and validators for text, `Password::with_confirmation` for the admin password
and plain `Password` for the superuser one (replacing `rpassword`), all under one
`ColorfulTheme`. With no terminal attached, interactive `oms init` fails up front with
the existing hint to use `--non-interactive` / `--password`, not mid-prompt.

```text
$ oms init

? Database
> Start PostgreSQL 16 in Docker for me   (Docker found)
  Use an existing PostgreSQL 16

  starting oms-postgres (postgres:16) on 127.0.0.1:5432 ... ready

Cockpit
  Admin password [Enter = generate]:
  Listen on      [localhost:3001]:

  connected to localhost:5432
  wrote ~/.config/oms/oms.toml (mode 0600)
  created database ods, role oms; applied 29 migrations; seeded reference data

Back up ~/.config/oms/oms.toml. ...
Cockpit login password: <shown only if generated>
Open http://localhost:3001/cockpit/ after starting the server:  oms
```

The menu is an arrow-key select (default highlighted). Choosing "existing" shows today's five Postgres prompts (host, port, database, superuser,
password) in place of the Docker step.

### Database choice

- Default is 1 when Docker is usable, else 2. "Usable" = `docker info` exits 0 within a
  few seconds. When it isn't, the menu still shows option 1 marked `(Docker not found)`
  / `(Docker not running)`; choosing it prints why and how to fix (install Docker, start
  the daemon, or add the user to the `docker` group) and asks again.
- Docker path, decided:
  - container `oms-postgres`, image `postgres:16`, named volume `oms-pgdata`,
    `--restart unless-stopped`, published on `127.0.0.1` only.
  - superuser `postgres` with a generated password passed via `POSTGRES_PASSWORD`;
    used for provisioning then dropped, same as a typed one. Not printed — after init
    the server runs as the `oms` role and migrations connect as it.
  - port 5432; if it's taken, use the first free port from 5433 up and say so.
  - wait for readiness by retrying the connection for up to 60 s, then fail with
    `docker logs oms-postgres` as the next step.
  - a container or volume named `oms-postgres`/`oms-pgdata` that already exists:
    refuse with a message naming both and the `docker rm` / `docker volume rm` to run.
    Never reuse or delete it silently (it may hold someone's data).
- The chosen Postgres connection is what `oms.toml` records; nothing in the config says
  "docker". The server doesn't manage the container afterwards.

### Admin password

- Typed without echo, then confirmed. Minimum 12 characters; shorter re-asks.
- Enter on the first prompt generates one (today's `generate_password`). Only a
  generated password is printed at the end.

### Listen address

- Default `localhost:3001`. Any `host:port` is accepted if it parses as a socket
  address or `localhost:<port>`.
- A non-loopback address (e.g. `0.0.0.0:3001`) prints one warning line: the cockpit and
  trading API become reachable from the network; prefer an SSH tunnel, Tailscale, or a
  reverse proxy with TLS.

### Non-interactive

New flags beside today's `DbArgs`:

| Flag | Env | Meaning |
|---|---|---|
| `--postgres docker\|existing` | — | Default `existing` (unchanged behaviour). |
| `--admin-password <pw>` | `OMS_ADMIN_PASSWORD` | Else generated and written to `oms.toml` only (unchanged). |
| `--bind <addr>` | `OMS_BIND_ADDR` | Default `localhost:3001`. |

Interactive mode uses the same flags as prompt defaults, as `DbArgs` already does;
`--admin-password` skips that prompt the way `--password` skips the superuser one.

## Installer

- Fix the stale closing text: `oms init`, not `oms database init`; drop the "from a
  clone of the repo, `docker compose up -d`" hint (init now does it).
- At the end, if `/dev/tty` is readable and the binary is runnable from where it was
  installed: `Run oms init now? [Y/n]` read from `/dev/tty`; yes runs it with
  stdin from `/dev/tty`. No tty (CI, `| sh` in a script) → print the next steps only.
- Skip the question when `OMS_NO_INIT=1`, or when a config already exists in any searched
  location (an update/reinstall, not a first run).

## Docs: one Getting started page

- `getting-started/index.mdx` titled **Getting started**, replacing
  `installation.md` and `first-run.md` (both become redirect stubs, as earlier renames
  did). Sidebar: Getting started → First paper trade → First API call.
- Sections: Requirements → Install → Set up (`oms init`) → Start → Open the cockpit →
  Next steps.
- Starlight `<Tabs syncKey="os">` with **macOS**, **Linux**, **From source**:
  - macOS: install line; `~/.local/bin` PATH line for `~/.zshrc`; Docker Desktop note.
  - Linux: install line; PATH line for `~/.bashrc`; OpenSSL 3 / glibc requirement and
    which distros lack it; Docker Engine + `docker` group note.
  - From source: toolchain (Rust, cmake, OpenSSL 3, Node 22) and the build commands.
- The prompts are shown as one `text` block like the flow above, with a short table of
  what each answer does.
- The script link points to the source on GitHub:
  `https://github.com/maxkuttner/openoms/blob/main/install.sh`.
- Installer options (`OMS_VERSION`, `OMS_INSTALL_DIR`, `OMS_NO_INIT`) and the
  non-interactive flags move to a collapsed `<details>` "Scripted installs".
- Configuration reference and CLI reference pages updated for the new flags/env vars.
- First paper trade's "assumes the server is running" link points at the new page.

## Testing

- dialoguer reads the terminal directly, so it stays a thin I/O layer behind a
  `Prompter` trait (select, input, password); decisions live in pure functions tested
  directly: menu default from Docker state, admin-password rule (min length,
  Enter-generates), bind parse and non-loopback warning, flag-seeded values skipping
  their prompts. The flow itself is tested with a scripted `Prompter`.
- Docker step behind a small trait (`docker_available`, `run_postgres`, `port_free`)
  so the decisions (port fallback, existing-container refusal, readiness timeout) are
  unit-tested without Docker. One `#[ignore]` test runs the real container.
- Installer: shellcheck, plus the existing install smoke workflow runs with no tty and
  must not hang.
- Docs: existing `docs-site` checks (link validator, build); old URLs still resolve.

## Left to the builder

- Exact wording of menu lines and warnings, within the shapes above.
- Whether the Docker probe shells out to `docker` (expected) or talks to the socket.
- Internal module split (e.g. `setup/init/{prompts,docker}.rs`).

## Calls I made (check these)

- Prompt library: `dialoguer` (chosen by the user), replacing `rpassword`.

- Docker Postgres binds `127.0.0.1` only and is not managed by the server afterwards.
- Generated Docker superuser password is never shown or stored.
- Admin password minimum 12 characters.
- Installer offers to run `oms init` via `/dev/tty`; `OMS_NO_INIT=1` opts out.
- `--postgres` defaults to `existing` in non-interactive mode, so current scripts keep
  working.
