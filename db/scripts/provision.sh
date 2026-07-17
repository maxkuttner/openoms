#!/usr/bin/env bash
#
# Provision the Postgres roles + the `ods` database. Everything this project needs
# beyond a running Postgres is created here — no infra/Ansible step.
#
#   mdm_master  owns the master catalog in `public` (instrument, venue, currency, …)
#   oms_user    owns schema `oms` (operational tables); reads `public`, writes the
#               catalog only where seeding needs it (see db/access/ods.sql)
#
# The split is least-privilege: the app role cannot drop the master catalog.
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
: "${MDM_MASTER_PASSWORD:?MDM_MASTER_PASSWORD must be set}"
: "${OMS_USER_PASSWORD:?OMS_USER_PASSWORD must be set}"
ODS_DB="${ODS_DB:-ods}"

# %I/%L quote the identifier/literal safely; \gexec runs the generated statement.
# mdm_master must exist before CREATE DATABASE ... OWNER mdm_master.
PGPASSWORD="$ADMIN_PASSWORD" psql -v ON_ERROR_STOP=1 \
  -h "$DB_HOST" -p "$DB_PORT" -U "$ADMIN_USER" -d postgres \
  -v mdm_pw="$MDM_MASTER_PASSWORD" -v oms_pw="$OMS_USER_PASSWORD" -v ods="$ODS_DB" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', 'mdm_master', :'mdm_pw') WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='mdm_master')\gexec
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', 'oms_user',   :'oms_pw') WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='oms_user')\gexec
SELECT format('CREATE DATABASE %I OWNER mdm_master', :'ods') WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname=:'ods')\gexec
SQL

echo "provisioned roles mdm_master(catalog-owner), oms_user(oms-owner) + database '$ODS_DB'"
