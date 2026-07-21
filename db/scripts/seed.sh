#!/usr/bin/env bash
#
# Post-deployment data seeding (idempotent). Run AFTER `make migrate` and
# `make access`, once the schema and grants exist. Seeds the reference/master
# data mdm owns; every seeder upserts, so this is safe to re-run.
#
# Seed order matters: currency + venue are FK targets for instrument, so they
# go first. Uses the committed snapshot under data/ for deterministic, offline
# venue seeding.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${DB_HOST:?DB_HOST must be set}"
: "${DB_PORT:?DB_PORT must be set}"
: "${ADMIN_USER:?ADMIN_USER must be set}"
: "${ADMIN_PASSWORD:?ADMIN_PASSWORD must be set}"
ODS_DB="${ODS_DB:-ods}"

psql_db() {
  local db="$1"; shift
  psql "postgres://${ADMIN_USER}:${ADMIN_PASSWORD}@${DB_HOST}:${DB_PORT}/${db}?sslmode=disable" \
    -v ON_ERROR_STOP=1 -q "$@"
}

echo "→ seeding currencies (ISO 4217 majors)"
psql_db "$ODS_DB" -f scripts/seed_currencies.sql

echo "→ seeding venues (ISO 10383 MIC registry)"
python3 scripts/seed_venues.py --source data/ISO10383_MIC.csv

echo "→ seeding crypto exchange venues (synthetic, non-MIC)"
psql_db "$ODS_DB" -f scripts/seed_crypto_venues.sql

# Instruments are seeded on demand, broker-first (not here, not scheduled):
# `make sync-broker BROKER=alpaca` creates the master instrument + broker_instrument
# rows; data feeds then derive what they can price from the catalog. Or
# `make db-fixtures` for the no-creds minimal set. All depend on currency + venue.

echo "post-deployment seeding complete."
