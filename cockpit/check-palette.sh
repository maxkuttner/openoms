#!/bin/sh
# Every colour in the cockpit and the trader UI comes from src/palette.ts, the
# one list of openoms colours. A hex literal anywhere else in src/ is a colour
# that drifted out of the scheme.
set -eu
cd "$(dirname "$0")"

[ -f src/palette.ts ] || { echo "src/palette.ts is missing" >&2; exit 1; }

if grep -rnE '#[0-9a-fA-F]{6}\b|#[0-9a-fA-F]{3}\b' src --include='*.ts' --include='*.tsx' --include='*.css' |
	grep -v '^src/palette\.ts:'; then
	echo "colours above are hard-coded; add them to src/palette.ts and import them" >&2
	exit 1
fi
echo "ok: every cockpit colour comes from src/palette.ts"
