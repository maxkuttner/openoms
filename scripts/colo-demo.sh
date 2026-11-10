#!/bin/sh
# Co-located strategy demo on Binance Spot testnet: a throwaway OMS (its own
# Postgres container and config) plus the reference market maker, wired
# together end to end.
#
#   scripts/colo-demo.sh up        # database, OMS, catalog, strategy, deployment
#   scripts/colo-demo.sh mm [args] # start the market maker (extra colo-example args)
#   scripts/colo-demo.sh stop-mm   # stop it (then record a verified stop to restart)
#   scripts/colo-demo.sh down      # stop everything and delete the demo state
#
# Needs: docker, python3, OpenSSL 3+ (`openssl` on PATH or Homebrew openssl@3),
# and a Binance Spot testnet Ed25519 key in $COLO_DEMO_KEYS (default
# ~/.config/openoms/binance-testnet): ed25519-private.pem and api-key. Keys
# stay in those files; they are passed to the OMS and the market maker only.
#
# State lives in $COLO_DEMO_DIR (default ./colo-demo); nothing else is changed.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIR=${COLO_DEMO_DIR:-$ROOT/colo-demo}
KEYS=${COLO_DEMO_KEYS:-$HOME/.config/openoms/binance-testnet}
PG=oms-colo-demo-pg
PG_PORT=${COLO_DEMO_PG_PORT:-55441}
HTTP=${COLO_DEMO_HTTP:-127.0.0.1:3021}
LINK_PORT=${COLO_DEMO_LINK_PORT:-9899}
STRATEGY=MM1

die() { echo "colo-demo: $*" >&2; exit 1; }

openssl3() {
  for o in openssl "$(brew --prefix openssl@3 2>/dev/null)/bin/openssl" "$(brew --prefix openssl@4 2>/dev/null)/bin/openssl"; do
    if command -v "$o" >/dev/null 2>&1 && "$o" version 2>/dev/null | grep -q '^OpenSSL [3-9]'; then
      echo "$o"; return
    fi
  done
  die "OpenSSL 3 or newer is required (brew install openssl@3)"
}

need_keys() {
  [ -s "$KEYS/ed25519-private.pem" ] && [ -s "$KEYS/api-key" ] ||
    die "no testnet key in $KEYS (ed25519-private.pem and api-key)"
}

build() {
  echo "building oms and colo-example…"
  (cd "$ROOT" && cargo build -q --bin oms && cargo build -q -p colo-example)
}

up() {
  need_keys
  command -v docker >/dev/null || die "docker is required"
  O=$(openssl3)
  [ -e "$DIR/oms.toml" ] && die "$DIR already holds a demo; run 'down' first"
  build
  mkdir -p "$DIR" && chmod 700 "$DIR"
  (umask 077
   "$O" rand -hex 16 > "$DIR/pgpw"
   "$O" rand -hex 16 > "$DIR/adminpw")

  echo "starting Postgres ($PG on 127.0.0.1:$PG_PORT)…"
  docker run --rm -d --name "$PG" -e POSTGRES_PASSWORD="$(cat "$DIR/pgpw")" \
    -p "127.0.0.1:$PG_PORT:5432" postgres:16 >/dev/null
  i=0; until [ "$(docker logs "$PG" 2>&1 | grep -c 'ready to accept connections')" -ge 2 ]; do
    i=$((i+1)); [ $i -lt 60 ] || die "Postgres did not start"; sleep 1
  done
  sleep 1

  echo "creating the OMS database and config…"
  OMS_CONFIG="$DIR/oms.toml" "$ROOT/target/debug/oms" init --non-interactive --postgres existing \
    --admin-password "$(cat "$DIR/adminpw")" --bind "$HTTP" --host 127.0.0.1 --port "$PG_PORT" \
    --password "$(cat "$DIR/pgpw")" --database ods_colo_demo >"$DIR/init.log" 2>&1 ||
    die "oms init failed; see $DIR/init.log"

  echo "making the control link's TLS certificate…"
  (umask 077; "$O" req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$DIR/link-key.pem" -out "$DIR/link.pem" -days 30 -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "extendedKeyUsage=serverAuth" \
    -addext "basicConstraints=critical,CA:FALSE" 2>/dev/null)
  printf '\n[control_link]\nport = %s\ncertificate = "%s"\nprivate_key = "%s"\n' \
    "$LINK_PORT" "$DIR/link.pem" "$DIR/link-key.pem" > "$DIR/link.toml"
  python3 -I - "$DIR/oms.toml" "$DIR/link.toml" <<'PY'
import sys
conf, link = sys.argv[1], open(sys.argv[2]).read()
s = open(conf).read().replace("[control_link]\n", link.lstrip("\n"), 1)
open(conf, "w").write(s)
PY

  echo "starting the OMS (http://$HTTP/cockpit/)…"
  OMS_CONFIG="$DIR/oms.toml" nohup "$ROOT/target/debug/oms" </dev/null >"$DIR/oms.log" 2>&1 &
  echo $! > "$DIR/oms.pid"
  i=0; until curl -s "http://$HTTP/health" 2>/dev/null | grep -q OK; do
    i=$((i+1)); [ $i -lt 60 ] || die "the OMS did not start; see $DIR/oms.log"; sleep 1
  done

  echo "reading the testnet account's opening balances…"
  balances "$O" > "$DIR/balances.json"

  echo "setting up the strategy and its deployment…"
  python3 -I "$ROOT/scripts/colo_demo_setup.py" "$DIR" "$KEYS" "http://$HTTP" "$STRATEGY"
  echo
  echo "ready. Cockpit: http://$HTTP/cockpit/  (admin password in $DIR/adminpw)"
  echo "start the market maker:  scripts/colo-demo.sh mm"
}

# The account's balances, signed with the testnet key (never printed).
balances() {
  O=$1
  q="omitZeroBalances=true&recvWindow=5000&timestamp=$(python3 -c 'import time;print(int(time.time()*1000))')"
  sig=$(printf %s "$q" | "$O" pkeyutl -sign -inkey "$KEYS/ed25519-private.pem" -rawin -in /dev/stdin | base64 |
    python3 -c 'import sys,urllib.parse;print(urllib.parse.quote(sys.stdin.read().strip(),safe=""))')
  curl -sf -H "X-MBX-APIKEY: $(cat "$KEYS/api-key")" \
    "https://testnet.binance.vision/api/v3/account?$q&signature=$sig" ||
    die "could not read testnet balances (key registered on testnet.binance.vision?)"
}

mm() {
  need_keys
  [ -e "$DIR/ids.json" ] || die "no demo; run 'up' first"
  [ -e "$DIR/mm.pid" ] && kill -0 "$(cat "$DIR/mm.pid")" 2>/dev/null && die "the market maker is already running"
  dep=$(python3 -I -c "import json,sys;print(json.load(open(sys.argv[1]))['deployment'])" "$DIR/ids.json")
  acc=$(python3 -I -c "import json,sys;print(json.load(open(sys.argv[1]))['account'])" "$DIR/ids.json")
  tok=$(python3 -I -c "import json,sys;print(json.load(open(sys.argv[1]))['token'])" "$DIR/token.json")
  mkdir -p "$DIR/journal"
  BINANCE_API_KEY="$(cat "$KEYS/api-key")" BINANCE_PRIVATE_KEY="$(cat "$KEYS/ed25519-private.pem")" \
    nohup "$ROOT/target/debug/colo-example" --oms-url "wss://localhost:$LINK_PORT" --deployment "$dep" \
      --token "$tok" --account-id "$acc" --account-ref testnet-mm1 --ca "$DIR/link.pem" \
      --journal-dir "$DIR/journal" --symbol BTCUSDT "$@" </dev/null >"$DIR/mm.log" 2>&1 &
  echo $! > "$DIR/mm.pid"
  echo "market maker started; log: $DIR/mm.log"
}

stop_mm() {
  [ -e "$DIR/mm.pid" ] && kill "$(cat "$DIR/mm.pid")" 2>/dev/null && echo "market maker stopped" || echo "not running"
  rm -f "$DIR/mm.pid"
}

down() {
  stop_mm >/dev/null || true
  [ -e "$DIR/oms.pid" ] && kill "$(cat "$DIR/oms.pid")" 2>/dev/null || true
  docker stop "$PG" >/dev/null 2>&1 || true
  case "$DIR" in */colo-demo) rm -rf "$DIR"; echo "demo removed" ;; *) echo "stopped; remove $DIR yourself" ;; esac
}

case "${1:-}" in
  up) up ;;
  mm) shift; mm "$@" ;;
  stop-mm) stop_mm ;;
  down) down ;;
  *) sed -n '2,16p' "$0"; exit 2 ;;
esac
