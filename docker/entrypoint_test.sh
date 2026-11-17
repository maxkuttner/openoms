#!/bin/sh
# Tests for docker/entrypoint.sh, with a fake `oms` on PATH that records how
# it was called. Run: sh docker/entrypoint_test.sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
script="$here/entrypoint.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir "$tmp/bin"
cat > "$tmp/bin/oms" <<'EOF'
#!/bin/sh
echo "[$*]" >> "$CALLS"
if [ "$*" = "database setup" ]; then exit "${FAKE_SETUP_EXIT:-0}"; fi
exit 0
EOF
chmod +x "$tmp/bin/oms"

fails=0
# run <name> <expected exit> <expected calls> [VAR=value ...] [-- args...]
run() {
    name=$1 want_exit=$2 want_calls=$3
    shift 3
    vars=""
    while [ $# -gt 0 ] && [ "$1" != "--" ]; do vars="$vars $1"; shift; done
    [ $# -gt 0 ] && shift
    : > "$tmp/calls"
    set +e
    # shellcheck disable=SC2086 # $vars is a list of VAR=value words on purpose
    env -i PATH="$tmp/bin:/usr/bin:/bin" CALLS="$tmp/calls" OMS_CONFIG="$tmp/none.toml" $vars \
        sh "$script" "$@" > "$tmp/out" 2> "$tmp/err"
    got_exit=$?
    set -e
    got_calls=$(tr '\n' ' ' < "$tmp/calls" | sed 's/ $//')
    if [ "$got_exit" != "$want_exit" ] || [ "$got_calls" != "$want_calls" ]; then
        echo "FAIL $name: exit $got_exit (want $want_exit), calls '$got_calls' (want '$want_calls')"
        sed 's/^/  stderr: /' "$tmp/err"
        fails=$((fails + 1))
    else
        echo "ok   $name"
    fi
}

run "no key refuses to start" 1 ""
grep -q OMS_MASTER_KEY "$tmp/err" || { echo "FAIL no-key message does not name OMS_MASTER_KEY"; fails=$((fails + 1)); }

run "key + superuser password: setup, then server" 0 "[database setup] []" \
    OMS_MASTER_KEY=base64:x POSTGRES_PASSWORD=p

printf '[oms]\nmaster_key = "base64:x"\n' > "$tmp/with-key.toml"
run "key in the config file, no superuser password: server only" 0 "[]" \
    OMS_CONFIG="$tmp/with-key.toml"
grep -q "skipping database setup" "$tmp/err" || { echo "FAIL no skip notice"; fails=$((fails + 1)); }

run "failed setup stops the start" 3 "[database setup]" \
    OMS_MASTER_KEY=base64:x POSTGRES_PASSWORD=p FAKE_SETUP_EXIT=3

run "arguments pass straight through" 0 "[init]" -- init
run "arguments with spaces pass through" 0 "[database status --host h]" -- database status --host h

[ "$fails" -eq 0 ] || { echo "$fails failing"; exit 1; }
echo "all passed"
