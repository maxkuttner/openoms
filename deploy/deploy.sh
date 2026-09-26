#!/usr/bin/env bash
# Ships the committed tree (HEAD, or $1 if given) to the test server and
# (re)builds+restarts it there. Run by hand from your workstation — never
# invoked by CI. First-time setup (oms.toml / .env on the server) is separate;
# see deploy/README.md.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${OMS_DEPLOY_HOST:-100.80.233.3}"
REF="${1:-HEAD}"
REMOTE_SRC="oms-deploy/src"

echo "shipping $REF to $HOST:~/$REMOTE_SRC ..."
ssh "$HOST" "mkdir -p ~/$REMOTE_SRC"
git archive "$REF" | ssh "$HOST" "rm -rf ~/$REMOTE_SRC/* && tar -x -C ~/$REMOTE_SRC"

echo "building and restarting..."
ssh "$HOST" "cd ~/$REMOTE_SRC && docker compose -f deploy/docker-compose.yml build && docker compose -f deploy/docker-compose.yml up -d"

echo "done. tailing logs (ctrl-c to stop watching, the container keeps running):"
ssh "$HOST" "docker logs -f oms-test-deploy"
