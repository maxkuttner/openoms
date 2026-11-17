#!/bin/sh
# Start script of the published image (ENTRYPOINT).
#
# With arguments (`docker compose run --rm oms init`, `oms database status`, ...)
# it runs `oms` with them unchanged.
#
# With none (starting the server), it first sets the database up, so the image
# runs from a compose file and environment variables alone:
#   1. refuses to start without a master key (OMS_MASTER_KEY, or
#      [oms].master_key in $OMS_CONFIG): without one, stored broker credentials
#      cannot be decrypted;
#   2. with the superuser password (POSTGRES_PASSWORD), runs
#      `oms database setup`: initializes a fresh database, migrates an existing
#      one, and stops here on any failure;
#   3. starts the server. The server itself never creates or migrates a database.
set -eu

if [ "$#" -gt 0 ]; then
    exec oms "$@"
fi

config=${OMS_CONFIG:-/config/oms.toml}
if [ -z "${OMS_MASTER_KEY:-}" ] && ! grep -Eq '^[[:space:]]*master_key[[:space:]]*=' "$config" 2>/dev/null; then
    echo "oms: no master key. Set OMS_MASTER_KEY (generate one with: echo \"base64:\$(openssl rand -base64 32)\")" >&2
    echo "     or provide [oms].master_key in $config." >&2
    exit 1
fi

if [ -n "${POSTGRES_PASSWORD:-}" ]; then
    oms database setup
else
    echo "oms: POSTGRES_PASSWORD not set; skipping database setup" >&2
fi

exec oms
