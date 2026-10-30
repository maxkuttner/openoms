#!/bin/sh
# Tests for render.sh. Run: sh packaging/homebrew/render_test.sh
# Checks are eval'd strings, so their $vars expand late on purpose.
# shellcheck disable=SC2016,SC2034
set -u
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi; }

printf '%s  oms-aarch64-apple-darwin.tar.gz\n%s  oms-x86_64-unknown-linux-gnu.tar.gz\n' \
	"$(printf 'a%.0s' $(seq 64))" "$(printf 'b%.0s' $(seq 64))" > "$tmp/good"
sh "$here/render.sh" 9.9.9 "$tmp/good" > "$tmp/out" 2> "$tmp/err"
check "good sums render" '[ $? -eq 0 ]'
check "version comes from the url, no redundant version line" '! grep -q "^ *version " "$tmp/out" && grep -q "download/v9.9.9/oms-x86_64" "$tmp/out"'
check "darwin hash lands" 'grep -q "sha256 \"$(printf "a%.0s" $(seq 64))\"" "$tmp/out"'
check "linux hash lands" 'grep -q "sha256 \"$(printf "b%.0s" $(seq 64))\"" "$tmp/out"'
check "urls carry the tag" 'grep -q "releases/download/v9.9.9/oms-aarch64-apple-darwin.tar.gz" "$tmp/out"'
check "no placeholder left" '! grep -q "{{" "$tmp/out"'

head -1 "$tmp/good" > "$tmp/nolinux"
if sh "$here/render.sh" 9.9.9 "$tmp/nolinux" > "$tmp/out2" 2> "$tmp/err2"; then rc=0; else rc=1; fi
check "missing linux line fails" '[ $rc -eq 1 ] && [ ! -s "$tmp/out2" ] && grep -q x86_64 "$tmp/err2"'

if sh "$here/render.sh" "" "$tmp/good" > "$tmp/out3" 2>/dev/null; then rc=0; else rc=1; fi
check "empty version fails" '[ $rc -eq 1 ] && [ ! -s "$tmp/out3" ]'

if sh "$here/render.sh" v9.9.9 "$tmp/good" > "$tmp/out4" 2>/dev/null; then rc=0; else rc=1; fi
check "a leading v fails" '[ $rc -eq 1 ]'

exit $fail
