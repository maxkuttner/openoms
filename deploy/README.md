# Manual test-server deploy

A hand-run deploy to a test box, deliberately kept out of `.github/workflows/`
— nothing here runs in CI. Builds the real release binary (cockpit bundle
embedded in, per `src/cockpit.rs`) in Docker and runs it with
`network_mode: host` against the box's own already-running native Postgres.

## One-time setup on the target box

```
ssh <host> mkdir -p ~/oms-deploy/config
scp oms.toml <host>:~/oms-deploy/config/oms.toml   # same master_key as prod/dev — see below
ssh <host> 'cat > ~/oms-deploy/config/.env' <<'EOF'
POSTGRES_PASSWORD=<the real value>
EOF
```

`oms.toml` on the server needs:

```toml
[oms]
master_key = "<same as whatever wrote the target database's encrypted columns>"

[database]
host = "<postgres host>"
port = 5432
username = "postgres"
database = "ods"

[server]
bind_addr = "0.0.0.0:3001"
admin_password = "<a fresh one — this is a bearer token, not tied to the DB>"
```

No `[auth.oidc]` block: this deploy skips login, admin console only (bearer
token). Add one later if the trade app's login flow needs testing here too.

**Important**: `master_key` decrypts already-stored secrets (broker
credentials, etc.) in whichever database this points at. If it's pointed at
a database another instance already wrote to, this must be the exact same
key, or every encrypted row in it fails to decrypt.

## Every deploy after that

```
./deploy/deploy.sh          # ships HEAD
./deploy/deploy.sh mybranch # ships any other ref
```

Rebuilds the image on the box and restarts the container. `oms.toml`/`.env`
are untouched by this — they live outside the shipped tree.

## Teardown

```
ssh <host> 'cd ~/oms-deploy/src && docker compose -f deploy/docker-compose.yml down'
```
