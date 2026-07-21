#!/usr/bin/env bash
#
# One-shot live instrument seeding: sync every broker whose creds are configured,
# then map every data feed onto the seeded instruments. Idempotent (every step
# upserts), so it is safe to re-run — on a schedule (cron) or by hand whenever the
# catalog should refresh (new listings, new option expiries). NOT run at server
# startup: it hits external broker APIs and can be slow.
#
# Steps whose creds are absent are skipped with a note, so a partial setup (e.g.
# Alpaca only) still works. Feed mapping needs no external creds — it maps whatever
# instruments already exist — but a feed with nothing seeded simply maps zero rows.
#
# Env knobs:
#   OPTION_UNDERLYINGS   comma-separated option underlyings for Alpaca (default: none)
#   ENRICH=1             run the OpenFIGI enrichment pass (FIGI/CUSIP on the master).
#                        Off by default — it is the slow phase and not needed to trade.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

run() { echo "→ oms setup $*"; cargo run --quiet -- setup "$@"; }
have() { [[ -n "${!1:-}" ]]; }

# Enrichment is opt-in (ENRICH=1); off by default it is the slow phase.
enrich_flag="--no-enrich"
[[ -n "${ENRICH:-}" ]] && enrich_flag=""

env_name_upper() { echo "${1:-PAPER}" | tr '[:lower:]' '[:upper:]'; }

# --- Brokers: the instrument source. ---
alpaca_env="$(env_name_upper "${ALPACA_ENV:-PAPER}")"
if have "ALPACA_${alpaca_env}_API_KEY" && have "ALPACA_${alpaca_env}_API_SECRET"; then
  run sync-broker --broker alpaca $enrich_flag ${OPTION_UNDERLYINGS:+--underlyings "$OPTION_UNDERLYINGS"}
else
  echo "· skip alpaca sync-broker (ALPACA_${alpaca_env}_API_KEY/SECRET not set)"
fi

binance_env="$(env_name_upper "${BINANCE_ENV:-PAPER}")"
if have "BINANCE_${binance_env}_API_KEY" && have "BINANCE_${binance_env}_PRIVATE_KEY_PATH"; then
  run sync-broker --broker binance $enrich_flag
else
  echo "· skip binance sync-broker (BINANCE_${binance_env}_API_KEY/PRIVATE_KEY_PATH not set)"
fi

# Feeds need no seeding step: each feed derives the instruments it can price from
# the catalog above, at startup, from its own symbology. Nothing to run here.

echo "✅ live seeding complete."
