#!/bin/sh
# Every colour in the cockpit and the trader UI comes from src/styles/tokens.css, the
# one list of openoms colours. A hex literal anywhere else in src/ is a colour
# that drifted out of the scheme.
set -eu
cd "$(dirname "$0")"

[ -f src/styles/tokens.css ] || { echo "src/styles/tokens.css is missing" >&2; exit 1; }

if grep -rnE '#[0-9a-fA-F]{6}\b|#[0-9a-fA-F]{3}\b' src --include='*.ts' --include='*.tsx' --include='*.css' |
	grep -v "^src/styles/tokens\.css:" |
	# shadcn's chart.tsx matches Recharts' own default strokes ([stroke='#ccc'],
	# [stroke='#fff']) to restyle them with tokens; those hexes are selectors, not colours.
	grep -vE "^src/components/ui/chart\.tsx:[0-9]+:.*\[stroke='#(ccc|fff)'\]"; then
	echo "colours above are hard-coded; add them to src/styles/tokens.css" >&2
	exit 1
fi
echo "ok: every cockpit colour comes from src/styles/tokens.css"
