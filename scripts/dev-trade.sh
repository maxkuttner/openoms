#!/usr/bin/env bash
# One command for the trader-webapp dev loop: brings up Postgres + the disposable
# Authentik (oidc profile), starts the OMS, starts the cockpit vite dev server, and
# opens the trade app. Ctrl-C tears down the two background processes it started
# (docker containers are left running — they're cheap to leave up between runs).
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose --profile oidc up -d

# First boot migrates Authentik's database and applies the `oms` blueprint, which can
# take a couple of minutes. Discovery answering means the provider exists.
echo "waiting for authentik..."
until curl -sf http://localhost:9000/application/o/oms/.well-known/openid-configuration >/dev/null 2>&1; do sleep 2; done

echo "waiting for postgres..."
until docker exec oms-postgres pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done

cargo run > /tmp/oms-server.log 2>&1 &
OMS_PID=$!
trap 'kill $OMS_PID $VITE_PID 2>/dev/null' EXIT

echo "waiting for the OMS..."
until curl -sf http://localhost:3001/auth/login -o /dev/null; do
  sleep 1
  if ! kill -0 $OMS_PID 2>/dev/null; then
    echo "OMS process died — see /tmp/oms-server.log"; exit 1
  fi
done

(cd cockpit && npm run dev > /tmp/cockpit-dev.log 2>&1) &
VITE_PID=$!

echo "waiting for vite..."
until curl -sf http://localhost:5173 -o /dev/null; do sleep 1; done

echo
echo "trade app:  http://localhost:5173/trade.html  (sign in through Authentik; invite yourself first)"
echo "admin console: http://localhost:5173/"
echo "authentik admin: http://localhost:9000/if/admin/  (akadmin / admin)"
echo "  the oms provider + application come from docker/authentik/blueprints/oms-dev.yaml;"
echo "  oms.toml: [auth.oidc] issuer = \"http://localhost:9000/application/o/oms/\", client_id = \"oms\""
echo "documentation page: https://openoms.dev/ (docs)"
echo
echo "OMS log: /tmp/oms-server.log · vite log: /tmp/cockpit-dev.log"
echo "ctrl-c to stop the OMS and vite (postgres/authentik stay up)"

open http://localhost:5173/trade.html 2>/dev/null || true

wait $OMS_PID $VITE_PID
