#!/bin/sh
# openoms installer.
#
#   curl -fsSL https://maxkuttner.github.io/openoms/install.sh | sh
#
# Downloads the released `oms` binary for this platform, verifies it against the
# release's SHA256SUMS, installs it into ~/.local/bin and, if that is not on PATH,
# adds it in your shell startup file. That is all it does: no sudo, no package
# manager, no Postgres, no config file, no server started. It prints the commands
# to run next — then, if a person is at the terminal, offers to run `oms init`
# (which asks before it creates anything).
#
#   OMS_VERSION          release tag to install (default: latest)
#   OMS_INSTALL_DIR      where the binary lands (default: $HOME/.local/bin)
#   OMS_NO_INIT=1        do not offer to run `oms init` at the end
#   OMS_NO_MODIFY_PATH=1 leave shell startup files alone; print the PATH line instead

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

main() {
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

	if ! "$INSTALL_DIR/oms" --version >/dev/null 2>&1; then
		rm -f "$INSTALL_DIR/oms"
		die "the installed binary will not run: $INSTALL_DIR/oms --version failed"
	fi
	printf 'installed %s to %s\n' "$("$INSTALL_DIR/oms" --version)" "$INSTALL_DIR/oms"

	add_to_path "$(uname -s)"

	cat <<-'EOF'

	next:

	    oms init     # asks where the database lives (it can start one in Docker),
	                 # sets the cockpit password, writes oms.toml
	    oms          # starts the OMS

	then open http://localhost:3001/cockpit/

	Later, `oms update` installs the newest release in place.
	EOF

	offer_init
}

# Puts INSTALL_DIR on PATH for new terminals by appending one line to the
# user's shell startup file, the way the Bun, Deno and rustup installers do.
# Skips a file that already has the line; OMS_NO_MODIFY_PATH=1 and shells it
# does not know print the line to add by hand instead. Takes `uname -s`.
add_to_path() {
	case ":$PATH:" in
	*":$INSTALL_DIR:"*) return 0 ;;
	esac

	# shellcheck disable=SC2016 # the literal $PATH belongs in the startup file
	line="$(printf 'export PATH="%s:$PATH"' "$INSTALL_DIR")"
	profile=""
	if [ "${OMS_NO_MODIFY_PATH:-}" != "1" ]; then
		case "${SHELL##*/}" in
		zsh) profile="${ZDOTDIR:-$HOME}/.zshrc" ;;
		# Terminal on macOS opens login shells, which read .bash_profile only.
		bash) if [ "$1" = "Darwin" ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi ;;
		fish)
			profile="$HOME/.config/fish/config.fish"
			line="fish_add_path \"$INSTALL_DIR\""
			;;
		esac
	fi

	if [ -z "$profile" ]; then
		printf '\n%s is not on your PATH. Add it to your shell startup file:\n\n    %s\n' \
			"$INSTALL_DIR" "$line"
		return 0
	fi

	if ! grep -qxF "$line" "$profile" 2>/dev/null; then
		mkdir -p "$(dirname "$profile")"
		printf '\n# openoms\n%s\n' "$line" >>"$profile"
	fi
	printf '\nadded %s to PATH in %s\nopen a new terminal to use oms, or run now:\n\n    %s\n' \
		"$INSTALL_DIR" "$profile" "$line"
}

# Ask once whether to run `oms init` now. Only when a person can answer: under
# `curl | sh` stdin is the script itself, so the question goes to /dev/tty, and
# with no terminal at all (CI, provisioning) there is nothing to ask.
offer_init() {
	[ "${OMS_NO_INIT:-}" = "1" ] && return 0
	if "$INSTALL_DIR/oms" config path >/dev/null 2>&1; then
		printf '\nexisting config found — skipping oms init\n'
		return 0
	fi
	(: </dev/tty) 2>/dev/null || return 0
	printf '\nRun oms init now? [Y/n] ' >/dev/tty
	read -r ans </dev/tty || return 0
	case "$ans" in
	"" | y | Y | yes | YES) "$INSTALL_DIR/oms" init </dev/tty >/dev/tty 2>&1 || true ;;
	*) ;;
	esac
}

# install_test.sh sources this file for its functions.
[ "${OMS_INSTALL_SH_NO_MAIN:-}" = "1" ] || main "$@"
