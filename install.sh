#!/bin/sh
# openoms installer.
#
#   curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
#
# Downloads the released `oms` binary for this platform, verifies it against the
# release's SHA256SUMS, and installs it into ~/.local/bin. That is all it does: no
# sudo, no package manager, no Postgres, no config file, no server started. It
# prints the commands to run next.
#
#   OMS_VERSION      release tag to install (default: latest)
#   OMS_INSTALL_DIR  where the binary lands (default: $HOME/.local/bin)

set -eu

REPO="maxkuttner/openoms"
VERSION="${OMS_VERSION:-latest}"
INSTALL_DIR="${OMS_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf 'error: %s\n' "$1" >&2
	exit 1
}

detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"

	# An x86_64 shell under Rosetta reports x86_64 on an arm64 Mac. Install the
	# native build regardless — it is the one that machine should be running.
	if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ] &&
		[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
		arch="arm64"
	fi

	case "$os/$arch" in
	Darwin/arm64) echo "aarch64-apple-darwin" ;;
	Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
	*) die "no prebuilt binary for $os/$arch — build from source: https://github.com/$REPO#setup" ;;
	esac
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "need sha256sum or shasum to verify the download"
	fi
}

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

TARGET="$(detect_target)"
TARBALL="oms-$TARGET.tar.gz"

if [ "$VERSION" = "latest" ]; then
	BASE="https://github.com/$REPO/releases/latest/download"
else
	BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

printf 'downloading %s (%s)\n' "$TARBALL" "$VERSION"
curl -fsSL "$BASE/$TARBALL" -o "$TMP/$TARBALL" || die "download failed: $BASE/$TARBALL"
curl -fsSL "$BASE/SHA256SUMS" -o "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"

expected="$(grep " $TARBALL\$" "$TMP/SHA256SUMS" | cut -d' ' -f1 || true)"
[ -n "$expected" ] || die "$TARBALL is not listed in SHA256SUMS"
actual="$(sha256_of "$TMP/$TARBALL")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $TARBALL
  expected $expected
  got      $actual"
printf 'checksum ok\n'

tar xzf "$TMP/$TARBALL" -C "$TMP"
[ -f "$TMP/oms" ] || die "$TARBALL did not contain an oms binary"

mkdir -p "$INSTALL_DIR"
if [ -e "$INSTALL_DIR/oms" ]; then
	printf 'replacing the existing %s/oms\n' "$INSTALL_DIR"
fi
install -m 755 "$TMP/oms" "$INSTALL_DIR/oms"

"$INSTALL_DIR/oms" --version >/dev/null 2>&1 ||
	die "the installed binary will not run: $INSTALL_DIR/oms --version failed"
printf 'installed %s to %s\n' "$("$INSTALL_DIR/oms" --version)" "$INSTALL_DIR/oms"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	# shellcheck disable=SC2016 # the literal $PATH is for the user to paste, not to expand here
	printf '\n%s is not on your PATH. Add it:\n\n    export PATH="%s:$PATH"\n\n(then put that line in ~/.zshrc or ~/.bashrc)\n' \
		"$INSTALL_DIR" "$INSTALL_DIR"
	;;
esac

cat <<'EOF'

next:

    oms database init     # creates the database and oms.toml — needs a Postgres 16
    oms                   # starts the OMS on localhost:3001

then open http://localhost:3001/cockpit/

No Postgres yet? From a clone of the repo, `docker compose up -d` brings one up on
127.0.0.1:5432 with the defaults `oms database init` already assumes.
EOF
