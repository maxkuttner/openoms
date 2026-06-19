#!/usr/bin/env bash
#
# Apply mdm-owned access policy (idempotent). Roles + databases are created by
# infra; this sets role attributes (search_path) and cross-schema grants.
# Run after `migrate` so GRANT ON ALL TABLES catches existing objects.
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

echo "→ applying role attributes (access/roles.sql)"
psql_db "$ODS_DB" -f access/roles.sql      # ALTER ROLE search_path is cluster-wide

echo "→ applying ods grants (access/ods.sql)"
psql_db "$ODS_DB" -f access/ods.sql

echo "access policy applied."
