---
title: Installation
description: Install the oms binary with one command, or build it from source.
---

openoms ships as a single `oms` binary. It is the server, the web cockpit and the
command-line tool in one file.

## Install

```sh
curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
```

The script downloads the latest release for your platform, verifies it against the
release's `SHA256SUMS`, and installs it to `~/.local/bin/oms`. That is all it does: no
sudo, no package manager, no database, no config file, nothing started.
[Read the script](/openoms/install.sh) before you pipe it into a shell.

Check it:

```sh
oms --version
```

### Not on your PATH?

`~/.local/bin` is not on the default macOS `PATH`. The installer tells you when it isn't
on yours and prints the line to add:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Put that line in `~/.zshrc` or `~/.bashrc` so new shells pick it up.

### Installer options

| Variable | Default | Effect |
|---|---|---|
| `OMS_VERSION` | `latest` | Release tag to install, e.g. `OMS_VERSION=v0.2.0` |
| `OMS_INSTALL_DIR` | `$HOME/.local/bin` | Where the binary goes |

```sh
curl -fsSL https://maxkuttner.github.io/openoms/install.sh | OMS_VERSION=v0.2.0 sh
```

## Supported platforms

Prebuilt binaries exist for exactly two platforms:

| Platform | Target | Requirements |
|---|---|---|
| macOS on Apple Silicon | `aarch64-apple-darwin` | none |
| Linux on x86_64 | `x86_64-unknown-linux-gnu` | glibc as on Ubuntu 22.04 / Debian 12 or newer, and OpenSSL 3 |

Debian 12 and Ubuntu 22.04+ have OpenSSL 3. Ubuntu 20.04, RHEL 8 and Amazon Linux 2 do
not. On any other platform, build from source.

You also need **PostgreSQL 16** somewhere the OMS can reach — see
[First run](/openoms/getting-started/first-run/).

## Updating

```sh
oms update
```

See [Updating](/openoms/operations/updating/).

## Build from source

You need:

- **Rust** (stable) — `curl https://sh.rustup.rs -sSf | sh`
- **cmake** and a C++ compiler — the embedded FIX engine is C++
- **OpenSSL 3** — on macOS, `brew install openssl@3`; Linux uses the system one
- **Node 22** — to build the cockpit web UI that gets embedded in the binary

```sh
git clone https://github.com/maxkuttner/openoms.git && cd openoms
(cd cockpit && npm ci && npm run build)   # the cockpit bundle the binary embeds
cargo build --release
./target/release/oms --version
```

Build the cockpit before `cargo build`: the bundle is embedded at compile time, and a
binary built without it serves a 404 at `/cockpit/`.

A binary built from source updates with `git pull` and a rebuild — `oms update` refuses
to overwrite it.
