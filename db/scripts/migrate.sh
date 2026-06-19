#!/usr/bin/env bash
#
# Central migration runner for the whole cluster.
#
# mdm owns all schema. Each "target" is a (database, schema, owner) tuple with
# its own directory of forward-only `NNNN_name.sql` files:
#
#   ods / public / mdm_master   → master data (instrument, venue, …)   "dbo"
#   ods / oms    / oms_user      → operational (account, order_*, …)
#   mds / public / market_user   → market data (equity_ohlcv_1d, …)
#
# The runner connects to each database as an admin/superuser, and for every
# migration: SET ROLE <owner> + search_path=<schema> so objects are created in
# the right schema AND owned by the right role; the tracking table
# (public._mdm_migrations) is owned and written by the admin.
#
# Usage: scripts/migrate.sh [up|status]   (default: up)
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

# Admin connection (a superuser or a role that is a member of every owner role,
# so SET ROLE works). Per-database name is appended per target below.
: "${DB_HOST:?DB_HOST must be set (see .env.example)}"
: "${DB_PORT:?DB_PORT must be set}"
: "${ADMIN_USER:?ADMIN_USER must be set (cluster admin / superuser)}"
: "${ADMIN_PASSWORD:?ADMIN_PASSWORD must be set}"

ODS_DB="${ODS_DB:-ods}"

# target = "db|schema|owner|dir"
# openoms owns only the ods layer; mds (market data) lives with the data pipeline.
TARGETS=(
  "${ODS_DB}|public|mdm_master|migrations/ods/public"
  "${ODS_DB}|oms|oms_user|migrations/ods/oms"
)

# psql against a given database as the admin role.
psql_db() {
  local db="$1"; shift
  psql "postgres://${ADMIN_USER}:${ADMIN_PASSWORD}@${DB_HOST}:${DB_PORT}/${db}?sslmode=disable" \
    -v ON_ERROR_STOP=1 "$@"
}

ensure_schema_and_tracking() {
  local db="$1" schema="$2" owner="$3"
  psql_db "$db" -q -c "CREATE SCHEMA IF NOT EXISTS ${schema} AUTHORIZATION ${owner};"
  psql_db "$db" -q -c "CREATE TABLE IF NOT EXISTS public._mdm_migrations (
        target     text NOT NULL,
        filename   text NOT NULL,
        applied_at timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (target, filename)
     );"
}

is_applied() {
  local db="$1" schema="$2" fn="$3"
  [[ "$(psql_db "$db" -tAc \
      "SELECT 1 FROM public._mdm_migrations WHERE target='${schema}' AND filename='${fn}';")" == "1" ]]
}

apply_file() {
  local db="$1" schema="$2" owner="$3" file="$4" fn="$5"
  echo "→ [${db}/${schema}] applying ${fn}"
  # One transaction: create objects as <owner> in <schema>, then record as admin.
  psql_db "$db" --single-transaction <<SQL
SET ROLE ${owner};
SET search_path TO ${schema};
\i ${file}
RESET ROLE;
INSERT INTO public._mdm_migrations (target, filename) VALUES ('${schema}', '${fn}');
SQL
}

cmd_up() {
  local applied=0 t db schema owner dir f fn
  for t in "${TARGETS[@]}"; do
    IFS='|' read -r db schema owner dir <<<"$t"
    ensure_schema_and_tracking "$db" "$schema" "$owner"
    shopt -s nullglob
    for f in "$dir"/*.sql; do
      fn="$(basename "$f")"
      is_applied "$db" "$schema" "$fn" && continue
      apply_file "$db" "$schema" "$owner" "$f" "$fn"
      applied=$((applied + 1))
    done
  done
  [[ "$applied" -eq 0 ]] && echo "Already up to date." || echo "Applied ${applied} migration(s)."
}

cmd_status() {
  local t db schema owner dir f fn state
  printf "%-8s %-8s %-28s %s\n" "DB" "SCHEMA" "MIGRATION" "STATUS"
  printf "%-8s %-8s %-28s %s\n" "--" "------" "---------" "------"
  for t in "${TARGETS[@]}"; do
    IFS='|' read -r db schema owner dir <<<"$t"
    ensure_schema_and_tracking "$db" "$schema" "$owner"
    shopt -s nullglob
    for f in "$dir"/*.sql; do
      fn="$(basename "$f")"
      is_applied "$db" "$schema" "$fn" && state="applied" || state="pending"
      printf "%-8s %-8s %-28s %s\n" "$db" "$schema" "$fn" "$state"
    done
  done
}

case "${1:-up}" in
  up)     cmd_up ;;
  status) cmd_status ;;
  *)      echo "usage: $0 [up|status]" >&2; exit 2 ;;
esac
