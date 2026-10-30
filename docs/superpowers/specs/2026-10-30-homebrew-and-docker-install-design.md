# Homebrew and Docker install channels — design

Date: 2026-10-30
Status: approved

## Why

The Installation page now has four tabs — macOS & Linux, Windows, Package Managers,
Docker — and two of them say "To be implemented". The goal is install as short as Bun's:
one command a person already knows how to run, and the same `oms init` → `oms` flow after
it. Homebrew is what a Mac user reaches for first; Docker is what a server operator
reaches for first.

## Goals

1. `brew install maxkuttner/tap/openoms` installs the release binary on macOS (Apple
   Silicon) and Linux x86_64, and `brew upgrade` moves it to each new release with no
   manual step from the maintainer.
2. `ghcr.io/maxkuttner/openoms` is a published image for `linux/amd64` and
   `linux/arm64`, tagged on every release.
3. A published `compose.yaml` runs openoms and PostgreSQL 16 together; from an empty
   directory to a logged-in cockpit is five copy-paste lines.
4. Both channels are built and published by the existing `v*` tag push — a release is
   still one tag.
5. The Installation page's Package Managers and Docker tabs (and Upgrading, Uninstall)
   describe the real commands; "To be implemented" goes away for these two.

## Non-goals

- pip / PyPI, Windows, Scoop, apt/rpm repositories. The Package Managers tab keeps a
  "pip: to be implemented" line.
- Getting the formula into homebrew-core, which needs notability (~75 stars) and a
  formula that builds from source. Later, separate step; the tap is the channel.
- Replacing `deploy/` — the maintainer's test server keeps its own Dockerfile and
  compose file.
- Kubernetes manifests or a Helm chart.
- Intel macOS in Homebrew (no release binary exists for it).

## Anti-goals

- A release whose binary, formula and image disagree on version. Each channel is checked
  against the tag, the way the binary already is.
- `oms update` overwriting a binary a package manager owns: brew or the image would
  silently undo it, or worse, brew's checksums would no longer match.
- A Docker setup whose cockpit is reachable from the network by default.
- A secret baked into the image or committed in `compose.yaml`.

## Homebrew

### Tap

- New public repo `maxkuttner/homebrew-tap`, holding `Formula/openoms.rb`. Users write
  `brew install maxkuttner/tap/openoms` (one command), or `brew tap maxkuttner/tap` once and then
  `brew install openoms`. The formula is named `openoms`; the command it installs is
  `oms`. Naming it `openoms` now means a later move to homebrew-core keeps the same
  install command.
- A **binary formula**: it downloads the release tarballs, it does not build. Per
  platform: `on_macos` + `on_arm` → `oms-aarch64-apple-darwin.tar.gz`; `on_linux` +
  `on_intel` → `oms-x86_64-unknown-linux-gnu.tar.gz`; each with its `sha256` from the
  release's `SHA256SUMS`. Other platforms get no `url`, so brew refuses with its own
  "not available" message.
- `install`: `bin.install "oms"`.
- `test do`: `oms --version` prints the formula's version.
- `caveats`: two lines — run `oms init`, then `oms`; link to Getting started.
- Linux keeps the release binary's requirement (glibc + OpenSSL 3 from the system); the
  formula does not try to vendor OpenSSL.

### Release automation

- The formula lives in this repo as a template, `packaging/homebrew/openoms.rb.tmpl`, with
  `{{version}}`, `{{sha256_darwin_arm64}}`, `{{sha256_linux_x86_64}}`.
- A new `homebrew` job in `release.yml`, after `publish`: reads `SHA256SUMS` from the
  release just created, renders the template, and pushes `Formula/openoms.rb` to the tap with
  commit message `openoms <version>`.
- Pushing to another repo needs a token: a fine-grained PAT with contents write on the
  tap only, stored as the secret `HOMEBREW_TAP_TOKEN` in this repo. The maintainer
  creates it once.
- The job fails the release (red, visible) if the rendered formula's version is not the
  tag or a checksum is missing. It does not retry or partially publish.

### `oms update` under Homebrew

- `oms update` refuses when the running binary's real path is inside a Homebrew Cellar
  (`/Cellar/openoms/` in the canonical path), with: `oms was installed by Homebrew — update
  it with: brew upgrade openoms`. Same shape as today's source-build refusal
  (`is_cargo_build`, `src/setup/update.rs:54`).
- `oms update --check` still works (it only reads).

### Install smoke

- `install-smoke.yml` gains a `brew` job on `macos-14`, after a release: `brew install
  maxkuttner/tap/openoms`, `oms --version` equals the tag, `brew test openoms`.

## Docker

### Image

- `ghcr.io/maxkuttner/openoms`, tags `X.Y.Z`, `X.Y`, `latest`. Multi-arch:
  `linux/amd64` and `linux/arm64`, each built natively (`ubuntu-22.04` and
  `ubuntu-22.04-arm` runners — no QEMU; the C++ FIX engine is slow under emulation),
  merged into one manifest.
- New `Dockerfile` at the repo root, multi-stage from source like `deploy/Dockerfile`
  (cockpit build → cargo build → `debian:bookworm-slim` + `ca-certificates libssl3`).
  Building from source, not copying the release tarball, because there is no arm64 Linux
  tarball.
- Runs as a non-root user `oms` (uid 10001). `ENTRYPOINT ["oms"]`, no `CMD` (so the
  default is the server, and `docker run … init` / `database migrate` work).
- Environment baked in: `OMS_CONFIG=/config/oms.toml`, `OMS_BIND_ADDR=0.0.0.0:3001`,
  `OMS_CONTAINER=1`. `VOLUME /config`, `EXPOSE 3001`.
- OCI labels: source repo, version, licence.
- A `release.yml` job builds and pushes it with `GITHUB_TOKEN` (`packages: write`), then
  runs the image's `oms --version` and fails if it is not the tag.
- The GHCR package must be made public once by hand after the first push (GitHub
  defaults new packages to private).

### `compose.yaml`

Published next to `install.sh` at `https://maxkuttner.github.io/openoms/compose.yaml`
(source: `docs-site/public/compose.yaml`).

- `postgres`: `postgres:16`, volume `oms-pgdata`, `POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?…}`
  (compose refuses to start without it, with a message saying to create `.env`), a
  `pg_isready` healthcheck. No published port — only `oms` reaches it.
- `oms`: `ghcr.io/maxkuttner/openoms:${OMS_VERSION:-latest}`, volume `oms-config:/config`,
  `ports: ["127.0.0.1:3001:3001"]` (loopback only on the host), `depends_on: postgres:
  condition: service_healthy`, `restart: unless-stopped`, and
  `POSTGRES_HOST=postgres`, `POSTGRES_USERNAME=postgres`,
  `POSTGRES_PASSWORD=${POSTGRES_PASSWORD}`.
- Volume names `oms-pgdata` and `oms-config` are scoped by compose's project name, so
  they never collide with the `oms-pgdata` volume `oms init --postgres docker` makes.

### The flow

```sh
mkdir openoms && cd openoms
curl -fsSLO https://maxkuttner.github.io/openoms/compose.yaml
echo "POSTGRES_PASSWORD=$(openssl rand -hex 24)" > .env
docker compose run --rm oms init
docker compose up -d
```

Then open `http://localhost:3001/cockpit/`.

### `oms init` inside the container (`OMS_CONTAINER=1`)

- No database menu: the source is `existing`, and host/user/password come from the
  environment compose sets, so those questions are skipped (they already are when a
  value is given; this adds the menu).
- No listen question: bind is `OMS_BIND_ADDR` (`0.0.0.0:3001`), and the non-loopback
  warning is not printed — the host side is published on `127.0.0.1` by compose. The
  summary says the cockpit is on `http://localhost:3001/cockpit/`.
- So it asks one thing: the admin password (Enter generates, shown at the end), as today.
- `oms init --non-interactive` works the same in the container, for scripted installs.

### Upgrading and `oms update`

- Upgrade: `docker compose pull && docker compose run --rm oms database migrate &&
  docker compose up -d`. `migrate` connects as the superuser from the environment, which
  compose provides.
- `oms update` refuses when `OMS_CONTAINER=1`: `oms runs in a container — update it
  with: docker compose pull && docker compose up -d`.

### Smoke

- A CI job (in `release.yml` after the image push, or `install-smoke.yml`) runs the flow
  above against the just-pushed tag with `oms init --non-interactive`, starts it, and
  checks `GET /cockpit/` returns 200 and `oms --version` in the container is the tag.

## Docs

- **Package Managers** tab: Homebrew command, plus a "pip: to be implemented" note.
- **Docker** tab: the five-line flow, a note that the cockpit is only on this machine
  (tunnel for remote), and where data lives (the two volumes).
- **Upgrading**: one line each for `brew upgrade openoms` and the compose upgrade.
- **Uninstall**: `brew uninstall openoms` (and `brew untap maxkuttner/tap`); `docker compose
  down` / `down -v` (the latter deletes the database and the master key, with the same
  caution as today).
- **Direct downloads**: unchanged.
- The `docs-site` test for the Installation page asserts both commands are present and
  that neither tab says "To be implemented".

## Testing

- `oms update` refusals: unit tests next to `a_cargo_build_is_recognised` for a Cellar
  path and for `OMS_CONTAINER=1`.
- Container-mode `oms init`: unit tests of the prompt flow with the scripted `Prompter`
  (only the admin password is asked; no exposure warning; bind is `0.0.0.0:3001`).
- Formula template: a script test renders it from a fixture `SHA256SUMS` and checks
  version and both checksums land; `brew audit --strict` runs on the rendered formula in
  CI.
- `compose.yaml`: `docker compose config` validates it in the docs check.
- The smoke jobs above are the end-to-end tests.

## Left to the builder

- Exact caveats, refusal and docs wording within the shapes above.
- Whether the formula render is a shell `sed` or a small script.
- Whether the Docker smoke lives in `release.yml` or `install-smoke.yml`.
- Docker layer caching strategy in CI.

## Calls I made (check these)

- Tap name `maxkuttner/homebrew-tap`, formula `openoms` (installs `oms`), a binary formula
  — not homebrew-core (approved by the user, with the rename).
- Tap updated by a release job using a PAT secret `HOMEBREW_TAP_TOKEN` you create once.
- Image name `ghcr.io/maxkuttner/openoms`, amd64 + arm64, built from source on native
  runners; runs as non-root.
- Docker install is `compose.yaml` + a `.env` you create with one `openssl` line —
  visible, no second install script. (A `curl … | sh` wrapper that does all five lines is
  a possible later step.)
- `OMS_CONTAINER=1` switches `oms init` to "admin password only" and makes `oms update`
  refuse.
- Cockpit published on host loopback only (`127.0.0.1:3001`).
