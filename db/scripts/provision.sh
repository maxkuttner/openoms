#!/usr/bin/env bash
#
# Provision the Postgres roles + the `ods` database for a standalone openoms install.
# In a managed deployment infra/Ansible does this; here it bootstraps a fresh Postgres
# so `make db-migrate` has roles to own schemas and a database to target.
#
# Idempotent: roles/database are only created if missing. Needs an ADMIN superuser.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
[[ -f .env ]] && { set -a; . ./.env; set +a; }

: "${DB_HOST:?DB_HOST must be set}"
: "${DB_PORT:?DB_PORT must be set}"
: "${ADMIN_USER:?ADMIN_USER must be set (a Postgres superuser)}"
: "${ADMIN_PASSWORD:?ADMIN_PASSWORD must be set}"
: "${OMS_USER_PASSWORD:?OMS_USER_PASSWORD must be set}"
ODS_DB="${ODS_DB:-ods}"

# %I/%L quote the identifier/literal safely; \gexec runs the generated statement.
PGPASSWORD="$ADMIN_PASSWORD" psql -v ON_ERROR_STOP=1 \
  -h "$DB_HOST" -p "$DB_PORT" -U "$ADMIN_USER" -d postgres \
  -v oms_pw="$OMS_USER_PASSWORD" -v ods="$ODS_DB" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', 'oms_user',    :'oms_pw')    WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='oms_user')\gexec
SELECT format('CREATE DATABASE %I OWNER mdm_master', :'ods') WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname=:'ods')\gexec
SQL

echo "provisioned roles postgres(admin), oms_user(oms-owner) + database '$ODS_DB'"
