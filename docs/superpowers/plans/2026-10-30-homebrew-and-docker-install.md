# Homebrew and Docker install channels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `brew install maxkuttner/tap/openoms` and a five-line `docker compose` flow both install openoms, published automatically by the `v*` tag push.

**Architecture:** Two small Rust changes make the binary aware of who owns it (`oms update` refuses under Homebrew or in a container) and of running in a container (`oms init` asks only the admin password). Packaging lives in `packaging/` (formula template + render script) and a root `Dockerfile`; `compose.yaml` ships from the docs site next to `install.sh`. `release.yml` gains `docker` and `homebrew` jobs; `install-smoke.yml` gains `brew` and `docker` jobs.

**Tech Stack:** Rust (clap, dialoguer), POSIX sh, Docker Buildx + GHCR, Homebrew formula (Ruby DSL), GitHub Actions, Starlight docs.

**Spec:** `docs/superpowers/specs/2026-10-30-homebrew-and-docker-install-design.md`

## Global Constraints

- Formula name `openoms`, installs the command `oms`; tap repo `maxkuttner/homebrew-tap`, file `Formula/openoms.rb`.
- Tap pushed with secret `HOMEBREW_TAP_TOKEN`; tap commit message `openoms <version>`.
- Image `ghcr.io/maxkuttner/openoms`, tags `X.Y.Z`, `X.Y`, `latest`; `linux/amd64` + `linux/arm64`, native runners `ubuntu-22.04` and `ubuntu-22.04-arm`, no QEMU.
- Image: non-root user `oms` uid 10001; `ENTRYPOINT ["oms"]`, no `CMD`; `OMS_CONFIG=/config/oms.toml`, `OMS_BIND_ADDR=0.0.0.0:3001`, `OMS_CONTAINER=1`; `VOLUME /config`; `EXPOSE 3001`.
- compose: cockpit published on `127.0.0.1:3001:3001` only; Postgres has no published port; `POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?…}`; volumes `oms-pgdata`, `oms-config`.
- Refusal copy: `oms was installed by Homebrew — update it with: brew upgrade openoms`; `oms runs in a container — update it with: docker compose pull && docker compose up -d`.
- Every channel is checked against the tag; a mismatch fails the release, red, no partial publish.
- Never touch the user's own `oms-postgres` container or `oms-pgdata` volume; local e2e uses compose project `omse2e` on a non-3001 port.
- Tests: `cargo test --bin oms` (no lib target); docs `cd docs-site && npm run check`.

## Review Focus

1. `oms update --check` under Homebrew or in a container must still report, not refuse — Task 1 test `check_still_reports_when_managed`.
2. Interactive container `oms init` must use `POSTGRES_PASSWORD` from the environment for the superuser (normally ignored interactively) — Task 2 test `container_takes_superuser_password_from_env`.
3. The `/config` named volume must be writable by uid 10001 on first use (a fresh named volume copies the image dir's ownership) — Task 3 step "non-root writes /config".
4. A `SHA256SUMS` missing one platform's line must fail the render, not emit an empty `sha256 ""` — Task 5 test case `missing linux line fails`.
5. `docker compose up` without `.env` must stop with a message naming `.env`, not start Postgres with no password — Task 4 step "no .env refuses".

---

### Task 1: `oms update` refuses when Homebrew or a container owns the binary

**Files:**
- Modify: `src/setup/update.rs:50-59` (beside `is_cargo_build`), `src/setup/update.rs:192-201` (`run`)

**Interfaces:**
- Produces: `fn managed_refusal(exe: &Path, in_container: bool) -> Option<String>` — `None` = update may proceed. Covers cargo build (existing copy), Homebrew Cellar, container. `fn in_container() -> bool` reads `OMS_CONTAINER == "1"`.

- [ ] **Step 1: Write failing tests** in `update.rs` `mod tests`:

```rust
#[test]
fn a_homebrew_install_is_refused_with_brew_upgrade() {
    let m = managed_refusal(Path::new("/opt/homebrew/Cellar/openoms/0.6.0/bin/oms"), false).unwrap();
    assert!(m.contains("brew upgrade openoms"), "{m}");
    assert!(managed_refusal(Path::new("/home/linuxbrew/.linuxbrew/Cellar/openoms/0.6.0/bin/oms"), false).is_some());
    assert!(managed_refusal(Path::new("/home/me/.local/bin/oms"), false).is_none());
    assert!(managed_refusal(Path::new("/opt/Cellar-backup/oms"), false).is_none());
}

#[test]
fn a_container_install_is_refused_with_compose_pull() {
    let m = managed_refusal(Path::new("/usr/local/bin/oms"), true).unwrap();
    assert!(m.contains("docker compose pull && docker compose up -d"), "{m}");
}

#[test]
fn a_cargo_build_is_still_refused_with_git_pull() {
    let m = managed_refusal(Path::new("/src/oms/target/release/oms"), false).unwrap();
    assert!(m.contains("git pull && cargo build --release"), "{m}");
}
```

Plus `check_still_reports_when_managed`: refactor so `run` only consults `managed_refusal` when `!args.check`; test via a pure `fn refusal_applies(check: bool) -> bool` or by asserting `run`'s ordering with a helper — pick the smallest seam; the assertion is "with `check: true`, a Cellar path yields no refusal".

- [ ] **Step 2: Run** `cargo test --bin oms update::` — Expected: FAIL, `managed_refusal` not found.
- [ ] **Step 3: Implement.** Cellar match = a path component exactly `Cellar` followed by `openoms`. Replace the inline `is_cargo_build` check in `run` with `managed_refusal(&exe, in_container())`, skipped when `args.check`. Keep `is_cargo_build` and its test.
- [ ] **Step 4: Run** `cargo test --bin oms update::` — Expected: PASS, all prior update tests too.
- [ ] **Step 5: Commit** `feat(update): refuse when Homebrew or a container owns the binary`.

### Task 2: Container-mode `oms init` asks only the admin password

**Files:**
- Modify: `src/setup/init/rules.rs` (new fn), `src/setup/init/prompts.rs:66-107` (`Defaults`, `collect`), `src/main.rs:~687-770` (Init dispatch)

**Interfaces:**
- Consumes: Task 1's `OMS_CONTAINER` convention (value `"1"`).
- Produces: `pub fn container_mode(var: Option<&str>) -> bool` in `rules.rs` (true only for `Some("1")`). `Defaults` gains `pub container: bool`.

Behaviour when `container`: source forced `Existing`, no menu; host/port/database/username taken from `Defaults` without asking; superuser password from `Defaults.superuser_password` without asking; admin password asked as today; bind = `d.bind`, no prompt, no exposure note. In `main.rs`, when `container_mode(env OMS_CONTAINER)`: `superuser_password` is seeded from `--password` else `POSTGRES_PASSWORD` (the env exception is container-only — keep the existing comment's rule for the host), Docker is never probed, and the non-interactive exposure warning is suppressed.

- [ ] **Step 1: Write failing tests** in `prompts.rs` tests, using the existing scripted `Prompter` and `defaults()` helper:

```rust
#[test]
fn container_asks_only_the_admin_password() {
    let mut d = defaults();
    d.container = true;
    d.superuser_password = Some("pg-secret".into());
    d.bind = "0.0.0.0:3001".into();
    let mut s = Scripted::new(vec![/* one password answer: "" (generate) */]);
    let a = collect(&mut s, &d, DockerStatus::NotFound).unwrap();
    assert!(matches!(a.db, DbChoice::Existing(ref p) if p.host == d.host && p.password == "pg-secret"));
    assert_eq!(a.bind, "0.0.0.0:3001");
    assert!(s.notes().is_empty(), "no exposure warning in a container");
    assert_eq!(s.asked(), vec!["Cockpit admin password"]); // label as used today
}

#[test]
fn container_takes_superuser_password_from_env() { /* main-side seeding: extract
   `fn superuser_password_default(flag: Option<String>, env: Option<String>, container: bool) -> Option<String>`
   into init/mod.rs; assert container+env → Some(env), host+env → None, flag wins in both */ }
```

and in `rules.rs`: `container_mode(Some("1"))` true; `None`, `Some("0")`, `Some("")` false. Adapt `Scripted` method names to whatever the existing test double exposes.
- [ ] **Step 2: Run** `cargo test --bin oms setup::init` — Expected: FAIL to compile (`container` field / fns missing).
- [ ] **Step 3: Implement** in `rules.rs`, `prompts.rs`, `init/mod.rs`, then wire `main.rs` (both interactive and non-interactive branches). Every existing `Defaults { … }` literal gets `container: false`.
- [ ] **Step 4: Run** `cargo test --bin oms` — Expected: whole suite PASS.
- [ ] **Step 5: Commit** `feat(init): container mode asks only the admin password`.

### Task 3: Root `Dockerfile`

**Files:**
- Create: `Dockerfile`, `.dockerignore` (exclude `target/`, `cockpit/node_modules`, `docs-site/node_modules`, `.git`, `*.toml` configs at root that are local — keep `Cargo.toml`)

**Interfaces:**
- Consumes: Tasks 1–2 behaviour via `OMS_CONTAINER=1`.
- Produces: image buildable with `docker build -t openoms:dev .`; used by Tasks 4 and 6.

Stages as `deploy/Dockerfile` (node 22 cockpit → `rust:1-bookworm` + cmake g++ pkg-config libssl-dev → `debian:bookworm-slim` + `ca-certificates libssl3`). Final stage creates user `oms` uid/gid 10001 and `mkdir /config && chown oms:oms /config` **before** `VOLUME /config`; `USER oms`; env, `EXPOSE`, `ENTRYPOINT` per Global Constraints; OCI labels `org.opencontainers.image.source=https://github.com/maxkuttner/openoms`, `.version` from `ARG VERSION`, `.licenses` from the repo's licence.

- [ ] **Step 1: Build** `docker build -t openoms:dev .` — Expected: success (≈10 min cold).
- [ ] **Step 2: Version** `docker run --rm openoms:dev --version` — Expected: `oms <Cargo.toml version>`.
- [ ] **Step 3: Non-root writes /config** `docker run --rm -v omse2e-cfg:/config --entrypoint sh openoms:dev -c 'id -u && touch /config/x'` then `docker volume rm omse2e-cfg` — Expected: `10001`, no permission error.
- [ ] **Step 4: Update refuses** `docker run --rm openoms:dev update` — Expected: exit 1, compose-pull message.
- [ ] **Step 5: Commit** `feat(docker): publishable openoms image`.

### Task 4: `compose.yaml` and local end-to-end

**Files:**
- Create: `docs-site/public/compose.yaml`
- Modify: `.github/workflows/build.yml` docs-site job (add `docker compose -f docs-site/public/compose.yaml config -q` with `POSTGRES_PASSWORD=x`)

Contents per spec "compose.yaml": services `postgres` (healthcheck `pg_isready -U postgres`, interval 2s, retries 30) and `oms` (`POSTGRES_HOST=postgres`, `POSTGRES_USERNAME=postgres`, `POSTGRES_PASSWORD=${POSTGRES_PASSWORD}`). The `:?` message: `create .env with POSTGRES_PASSWORD=… — see https://maxkuttner.github.io/openoms/getting-started/installation/`.

- [ ] **Step 1: Validate** `POSTGRES_PASSWORD=x docker compose -f docs-site/public/compose.yaml config -q` — Expected: exit 0.
- [ ] **Step 2: No .env refuses** same command without the var — Expected: non-zero, message names `.env`.
- [ ] **Step 3: End-to-end** in the scratchpad: copy compose.yaml, add `compose.override.yaml` setting `image: openoms:dev` and port `127.0.0.1:3911:3001`, `.env` with a random password, then `docker compose -p omse2e run --rm oms init --non-interactive`, `docker compose -p omse2e up -d`, poll `curl -fsS http://127.0.0.1:3911/cockpit/` — Expected: 200 within 60 s; init printed no exposure warning.
- [ ] **Step 4: Migrate path** `docker compose -p omse2e run --rm oms database migrate` — Expected: exit 0, "up to date".
- [ ] **Step 5: Clean up** `docker compose -p omse2e down -v` — only project `omse2e` volumes; confirm `oms-postgres`/`oms-pgdata` untouched with `docker ps -a`/`docker volume ls`.
- [ ] **Step 6: Commit** `feat(docker): compose.yaml for openoms + PostgreSQL 16`.

### Task 5: Homebrew formula template and render script

**Files:**
- Create: `packaging/homebrew/openoms.rb.tmpl`, `packaging/homebrew/render.sh`, `packaging/homebrew/render_test.sh`
- Modify: `.github/workflows/build.yml:119` (shellcheck the two scripts; run `render_test.sh`)

**Interfaces:**
- Produces: `render.sh <version-without-v> <path-to-SHA256SUMS>` → formula on stdout; exit 1 with a message if a checksum line is missing or version is empty. Placeholders `{{version}}`, `{{sha256_darwin_arm64}}`, `{{sha256_linux_x86_64}}`.

Formula: `class Openoms < Formula`, `desc`, `homepage "https://maxkuttner.github.io/openoms/"`, `version`, `license` (repo's), `on_macos { on_arm { url …/releases/download/v{{version}}/oms-aarch64-apple-darwin.tar.gz; sha256 … } }`, `on_linux { on_intel { url …x86_64-unknown-linux-gnu.tar.gz; sha256 … } }`, `def install; bin.install "oms"; end`, `def caveats` (run `oms init`, then `oms`; link Getting started), `test do assert_match version.to_s, shell_output("#{bin}/oms --version") end`.

- [ ] **Step 1: Write `render_test.sh`** with cases: good fixture → output contains `version "9.9.9"` and both hashes; `missing linux line fails` → exit non-zero, no output file; empty version fails.
- [ ] **Step 2: Run** `sh packaging/homebrew/render_test.sh` — Expected: FAIL (render.sh missing).
- [ ] **Step 3: Implement** `render.sh` (POSIX sh, `sed` substitution; hashes looked up by exact file name, like `expected_sha256` in `update.rs`).
- [ ] **Step 4: Run** the test, `shellcheck packaging/homebrew/*.sh`, and on this Mac `brew style` on a rendered formula (`sh render.sh 0.5.0 <(gh release download v0.5.0 -p SHA256SUMS -O -) > $SCRATCH/openoms.rb && brew style $SCRATCH/openoms.rb`) — Expected: all clean.
- [ ] **Step 5: Commit** `feat(brew): openoms formula template and renderer`.

### Task 6: Release and smoke workflows

**Files:**
- Modify: `.github/workflows/release.yml` (jobs `docker`, `docker-manifest`, `homebrew`), `.github/workflows/install-smoke.yml` (jobs `brew`, `docker`)

**Interfaces:**
- Consumes: Task 3 `Dockerfile` (`ARG VERSION`), Task 4 `compose.yaml`, Task 5 `render.sh`.

- `docker` (matrix `ubuntu-22.04`→amd64, `ubuntu-22.04-arm`→arm64; `permissions: packages: write`): buildx build with `VERSION=${GITHUB_REF_NAME#v}`, push by digest; check `docker run <image> --version` equals the tag before pushing.
- `docker-manifest` (needs `docker`): `docker buildx imagetools create` tags `X.Y.Z`, `X.Y`, `latest`.
- `homebrew` (needs `publish`, `runs-on: ubuntu-22.04`): `gh release download "$GITHUB_REF_NAME" -p SHA256SUMS`, render, check `version "<tag>"` present, clone `maxkuttner/homebrew-tap` with `HOMEBREW_TAP_TOKEN`, write `Formula/openoms.rb`, commit `openoms <version>`, push.
- smoke `brew` (`macos-14`): `brew install maxkuttner/tap/openoms`, version equals latest tag, `brew test openoms`, then `oms update` exits 1 with `brew upgrade openoms`.
- smoke `docker` (`ubuntu-22.04`): the five-line flow from the spec with `init --non-interactive`, `/cockpit/` 200, container `oms --version` equals tag.

- [ ] **Step 1:** `actionlint` both files (install via `brew install actionlint` if absent) — Expected: clean.
- [ ] **Step 2:** Commit `ci(release): publish the Docker image and Homebrew formula`. Real verification is the next release (see Maintainer steps).

### Task 7: Docs

**Files:**
- Modify: `docs-site/src/content/docs/getting-started/installation.mdx` (Package Managers, Docker tabs; Upgrading; Uninstall), `docs-site/test/dist.test.mjs` (installation test), `docs-site/src/content/docs/getting-started/index.mdx` (one line: "or `brew install maxkuttner/tap/openoms`, or Docker — see Installation")

- Package Managers tab: Homebrew command (both forms: one-shot and `brew tap` + `brew install openoms`), note that it installs `oms`; pip stays a "To be implemented" aside.
- Docker tab: the five lines, cockpit only on this machine (tunnel for remote), data in volumes `oms-pgdata` and `oms-config`.
- Upgrading: `brew upgrade openoms`; `docker compose pull && docker compose run --rm oms database migrate && docker compose up -d`.
- Uninstall: Package Managers tab `brew uninstall openoms` + `brew untap maxkuttner/tap`; add a Docker tab: `docker compose down` / `down -v` with the master-key caution.

- [ ] **Step 1: Extend the test** — installation page includes `brew install maxkuttner/tap/openoms` and `docker compose run --rm oms init`; the Docker tab panel does not contain `To be implemented`.
- [ ] **Step 2: Run** `cd docs-site && npm run check` — Expected: FAIL on the new assertions.
- [ ] **Step 3: Edit the pages.**
- [ ] **Step 4: Run** `npm run check` — Expected: PASS.
- [ ] **Step 5: Commit** `docs(install): Homebrew and Docker tabs`.

## Maintainer steps (need the user — not in any task)

1. Create public repo `maxkuttner/homebrew-tap` with an empty `Formula/` (`gh repo create maxkuttner/homebrew-tap --public`) — ask before running.
2. Create a fine-grained PAT, contents write on that repo only; save as secret `HOMEBREW_TAP_TOKEN` in `maxkuttner/openoms`. User does this (secret handling).
3. After the first release with these jobs: make the GHCR package `openoms` public (GitHub → Packages → settings).
4. Cut that release only after merge — docs advertise both channels as soon as main publishes.
