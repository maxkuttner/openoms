#!/usr/bin/env bash
#
# Load the minimal no-creds fixture (SPY) so a fresh install has one tradeable
# instrument without any broker/data-provider credentials. Idempotent — the fixture
# upserts on natural keys (ON CONFLICT DO NOTHING).
#
# Extracted from the `db-fixtures` make recipe so boot-time bootstrap can orchestrate
# it the same way as provision/migrate/access/seed. Needs an ADMIN role (the fixture
# writes master tables owned by mdm_master).
set -euo pipefail

# Repo root is two levels up (db/scripts → db → repo); the fixture lives under the
# top-level scripts/ dir, not db/scripts/.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
[[ -f .env ]] && { set -a; . ./.env; set +a; }

: "${DB_HOST:?DB_HOST must be set}"
: "${DB_PORT:?DB_PORT must be set}"
: "${ADMIN_USER:?ADMIN_USER must be set}"
: "${ADMIN_PASSWORD:?ADMIN_PASSWORD must be set}"
ODS_DB="${ODS_DB:-ods}"

echo "→ loading minimal fixture (SPY)"
PGPASSWORD="$ADMIN_PASSWORD" psql -v ON_ERROR_STOP=1 \
  -h "$DB_HOST" -p "$DB_PORT" -U "$ADMIN_USER" -d "$ODS_DB" \
  -f scripts/fixtures/minimal_seed.sql

echo "fixture loaded."
