#!/usr/bin/env bash
# One command for the trader-webapp dev loop: brings up Postgres + the disposable
# Keycloak (oidc profile), starts the OMS, starts the cockpit vite dev server, and
# opens the trade app. Ctrl-C tears down the two background processes it started
# (docker containers are left running — they're cheap to leave up between runs).
set -euo pipefail
cd "$(dirname "$0")/.."

docker compose --profile oidc up -d

echo "waiting for keycloak..."
until curl -sf http://localhost:8090/realms/oms-dev >/dev/null 2>&1; do sleep 1; done

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
echo "trade app:  http://localhost:5173/trade.html  (sign in as trader / trader)"
echo "admin console: http://localhost:5173/"
echo "keycloak admin: http://localhost:8090  (admin / admin)"
echo
echo "OMS log: /tmp/oms-server.log · vite log: /tmp/cockpit-dev.log"
echo "ctrl-c to stop the OMS and vite (postgres/keycloak stay up)"

open http://localhost:5173/trade.html 2>/dev/null || true

wait $OMS_PID $VITE_PID
