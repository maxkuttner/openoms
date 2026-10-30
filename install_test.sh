#!/bin/sh
# Tests install.sh's PATH setup: add_to_path, run against a throwaway $HOME.
#
#   sh install_test.sh
# shellcheck disable=SC2016 # literal $PATH in expected file contents

set -u
cd "$(dirname "$0")"

fails=0
fail() {
	printf 'FAIL: %s\n' "$1"
	fails=$((fails + 1))
}

# Each case runs add_to_path in a subshell with a fresh HOME, so nothing leaks
# between cases and nothing touches the real home directory.
run() { # <shell> <os> [env assignments...]
	shell="$1"
	os="$2"
	shift 2
	(
		HOME="$T"
		SHELL="$shell"
		PATH="/usr/bin:/bin"
		unset ZDOTDIR OMS_NO_MODIFY_PATH
		for kv in "$@"; do export "${kv?}"; done
		INSTALL_DIR="$HOME/.local/bin"
		OMS_INSTALL_SH_NO_MAIN=1
		. ./install.sh
		add_to_path "$os"
	) >"$T/out" 2>&1
}

fresh() {
	T="$(mktemp -d)"
}

line() { printf 'export PATH="%s/.local/bin:$PATH"' "$T"; }

# 1. zsh writes ~/.zshrc and says so.
fresh
run /bin/zsh Darwin
grep -qxF "$(line)" "$T/.zshrc" 2>/dev/null || fail "zsh: ~/.zshrc lacks the PATH line"
grep -q "$T/.zshrc" "$T/out" || fail "zsh: output does not name ~/.zshrc"
grep -q "new terminal" "$T/out" || fail "zsh: output does not say to open a new terminal"

# 2. zsh honours ZDOTDIR.
fresh
mkdir -p "$T/zdot"
run /usr/bin/zsh Linux "ZDOTDIR=$T/zdot"
grep -qxF "$(line)" "$T/zdot/.zshrc" 2>/dev/null || fail "zsh: ZDOTDIR/.zshrc lacks the PATH line"
[ ! -e "$T/.zshrc" ] || fail "zsh: wrote ~/.zshrc despite ZDOTDIR"

# 3. A second install does not add the line twice.
fresh
run /bin/zsh Darwin
run /bin/zsh Darwin
n="$(grep -cxF "$(line)" "$T/.zshrc")"
[ "$n" = 1 ] || fail "zsh: PATH line written $n times on reinstall"

# 4. bash on Linux writes ~/.bashrc.
fresh
run /bin/bash Linux
grep -qxF "$(line)" "$T/.bashrc" 2>/dev/null || fail "bash/Linux: ~/.bashrc lacks the PATH line"

# 5. bash on macOS writes ~/.bash_profile: Terminal opens login shells.
fresh
run /bin/bash Darwin
grep -qxF "$(line)" "$T/.bash_profile" 2>/dev/null || fail "bash/macOS: ~/.bash_profile lacks the PATH line"

# 6. fish uses fish_add_path in its own config.
fresh
run /usr/local/bin/fish Linux
grep -qxF "fish_add_path \"$T/.local/bin\"" "$T/.config/fish/config.fish" 2>/dev/null ||
	fail "fish: config.fish lacks fish_add_path"

# 7. An unknown shell gets the manual hint and no file.
fresh
run /bin/tcsh Linux
for f in "$T"/.[!.]* "$T"/*; do
	[ -e "$f" ] && [ "$f" != "$T/out" ] && fail "tcsh: wrote $f"
done
grep -qF "$(line)" "$T/out" || fail "tcsh: no manual PATH hint"

# 8. OMS_NO_MODIFY_PATH=1 opts out, with the manual hint.
fresh
run /bin/zsh Darwin OMS_NO_MODIFY_PATH=1
[ ! -e "$T/.zshrc" ] || fail "opt-out: wrote ~/.zshrc"
grep -qF "$(line)" "$T/out" || fail "opt-out: no manual PATH hint"

# 9. Already on PATH: nothing written, nothing printed.
fresh
run /bin/zsh Darwin "PATH=$T/.local/bin:/usr/bin:/bin"
[ ! -e "$T/.zshrc" ] || fail "on PATH: wrote ~/.zshrc"
[ ! -s "$T/out" ] || fail "on PATH: printed $(cat "$T/out")"

if [ "$fails" -gt 0 ]; then
	printf '%s failed\n' "$fails"
	exit 1
fi
printf 'install_test: 9 cases ok\n'
