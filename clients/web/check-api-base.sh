#!/bin/sh
# The bundled cockpit is served same-origin by the OMS, so a built bundle must not
# contain the "/api" dev-proxy prefix. Run after `npm run build`.
set -eu
cd "$(dirname "$0")"
[ -d dist ] || { echo "no dist/ — run npm run build first" >&2; exit 1; }

# Vite 8's default (rolldown/oxc) minifier rewrites string literals to template
# literals, so quotes around a path can be backticks as well as double quotes —
# match either.
if grep -rq '[`"]/api/' dist/assets; then
  echo "built bundle still calls the /api dev proxy:" >&2
  grep -ro '[`"]/api/[a-z-]*' dist/assets | sort -u >&2
  exit 1
fi

grep -rq '/health[`"]' dist/assets \
  || { echo "built bundle does not call /health — API_BASE wiring is wrong" >&2; exit 1; }

echo "ok: bundle calls the OMS same-origin"
