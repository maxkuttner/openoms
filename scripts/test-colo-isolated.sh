#!/usr/bin/env bash
# Run the co-located failure suite only against an explicitly selected,
# disposable Postgres. Never reads oms.toml and never falls back to the shared
# development configuration.
set -euo pipefail

: "${COLO_TEST_DATABASE_URL:?set COLO_TEST_DATABASE_URL to a disposable, already-migrated Postgres}"
db="${COLO_TEST_DATABASE_URL%%\?*}"
db="${db##*/}"
case "$db" in
  *test*|*colo*|*scratch*) ;;
  *) echo "refusing database '$db': name must contain test, colo or scratch" >&2; exit 2 ;;
esac

export OMS_CONFIG=/nonexistent
unset POSTGRES_PASSWORD || true

echo "== local runtime system checks =="
cargo test -p colo-runtime --test system

echo "== durable deployment / inbox / admission (isolated DB: $db) =="
cargo test -p rustoms colo::deployment::tests -- --ignored
cargo test -p rustoms colo::ingest::tests -- --ignored
cargo test -p rustoms risk_engine::tests -- --ignored

echo "isolated co-located checks passed"
