# Connection configuration from the cockpit

## Problem

Configuring a broker or a market-data feed means editing `.env` and restarting the
process. There is no other way.

Every credential is read from the environment exactly once, at boot, and wired
straight into the adapter registry (`main.rs:454-542`) and the feed sessions
(`main.rs:609`). `Broker::has_creds()` (`brokers.rs:64`) is a literal `env::var`
check. `broker_connection` exists as a database row but its own comment concedes
that "credentials live in env ({BROKER}\_{ENV}\_{KEY})" — connection identity is in
Postgres, authentication is in a file, and keeping them consistent is manual work.

The consequences a new user meets, in order:

1. The first login password is itself an environment variable
   (`OMS_ADMIN_PASSWORD`). There is no "create your admin account" screen.
2. Broker and feed credentials must be hand-copied from `.env.example`, with the
   correct `{BROKER}_{ENV}_{KEY}` spelling and no validation of either.
3. `BinanceFix` wants a *path to a private key file*
   (`BINANCE_PAPER_PRIVATE_KEY_PATH`), which no GUI can supply.
4. Nothing tests a credential. Wrong keys surface as a log line during a stream
   reconnect, minutes later.
5. `BrokerConnections.tsx` and `DataFeeds.tsx` are read-only viewers; no write path
   exists anywhere in the cockpit.
6. Filling the instrument catalog is CLI-only (`setup sync-broker`) or a silent
   background job. No button, no progress, no completion signal.
7. Nothing knows whether setup is finished. Preflight warns into stdout.

**Goal:** a professional user configures every external connection from the
cockpit. Environment and config files hold only what is genuinely chicken-and-egg —
how to reach the database, and the key that protects everything else.

## Scope

**In:** brokers (Alpaca REST, Binance FIX, IBKR FIX) and market-data feeds
(Databento). These share one shape — credentials, a live adapter, a meaningful
connection test, a reload — and that shape is what the design must get right.

**Out, deliberately:**

- **Kafka and OpenFIGI.** Same shape as the above; they drop in once the pattern
  exists, and including them now widens the first implementation without teaching
  anything new.
- **Behaviour flags** (`OMS_SYNC_ON_BOOT`, `OMS_SYNC_UNDERLYINGS`,
  `OMS_ENABLE_POSITION_PROJECTOR`). These are not connections. A general settings
  system and a secrets system want different designs; building both at once
  produces a worse version of each.
- **User accounts.** The single shared admin password stays for now. Config rows
  reserve `credentials_updated_by`, left null until accounts land, so the audit
  trail is structurally present before it is populated.
- **Sync progress tracking.** No `sync_run` table. The setup checklist polls for a
  non-empty catalog instead.

## Decisions

Recorded with their reasoning, because each closed off a plausible alternative.

**Bootstrap lives in `oms.toml`, not `.env`.** Some configuration must exist before
the database can be read, and an encrypted store needs a key that is not inside the
thing it encrypts. Those are the only survivors. Environment variables remain
*supported* as an override — that is how Docker, systemd and CI inject
configuration — but a human never has to touch them.

**One encrypted blob per connection, not a field-per-row secret store.** The
database stores an opaque `BYTEA`; `serde` handles the shape on the Rust side. A
per-field store buys finer-grained rotation that nobody has asked for, at the cost
of joins, more crypto operations, and a metadata layer describing which fields each
connection needs.

**No form-rendering engine.** An earlier draft had a descriptor per connection kind
that both API validation and the cockpit form derived from. With four connection
kinds, a small hardcoded form per broker is less machinery and more readable. The
duplication is two places agreeing on field names, which the serde enum makes
obvious when it drifts.

**Credentials apply live.** `stream_supervisor` already treats a dropped stream as
something to reconnect with backoff, so restarting a feed extends a mechanism that
exists. "Saved — now restart" would leave the experience feeling like editing a
config file with extra steps.

**The superuser password is prompted, not persisted.** `oms.toml` keeps the
database host, port, name and superuser *name*. The credential that can
`DROP DATABASE` does not sit on disk between uses. `serve` never needs it; only
`migrate` and `drop` prompt.

**Environment variables stop being consulted for *credentials* entirely** once
imported. No fallback and no precedence rules, so there is exactly one place to
look when something will not authenticate.

This does not contradict the previous point: the environment keeps overriding
**bootstrap** settings (database host, bind address, master key) because that is
how containers inject them. It stops being a source of **broker and feed
credentials**, which move to the database. The two never overlap.

## Design

### Bootstrap: `oms init`

A new interactive command. Everything typed is the connection to the user's
Postgres; everything else is generated.

```
$ oms init

Postgres host      [localhost]:
Postgres port      [5432]:
Database name      [ods]:
Superuser name     [postgres]:
Superuser password: ********

✓ connected to localhost:5432
✓ generated oms role password
✓ generated master key
✓ wrote oms.toml (mode 0600)
✓ added oms.toml to .gitignore
✓ created role oms, database ods
✓ applied 43 migrations
✓ seeded 2858 venues, 25 currencies, 10 calendars

Back up oms.toml. The master key in it is the only thing that can
decrypt stored broker credentials — lose it and they are gone.

Start the server:  oms
```

```toml
# oms.toml
[database]
host     = "localhost"
port     = 5432
username = "postgres"        # superuser name; password is prompted
database = "ods"

[oms]
password   = "…"             # generated; the role the server connects as
master_key = "base64:…"      # generated; 32 bytes from the OS CSPRNG

[server]
bind_addr      = "localhost:3001"
admin_password = "…"         # cockpit login; unchanged mechanism, new home
```

The admin password moves from `OMS_ADMIN_PASSWORD` into `oms.toml` but is otherwise
untouched — still one shared secret, still the same check. `setup-status` reports it
as `"default"` until changed, which is the prompt that eventually justifies real
accounts. Moving it now costs nothing and keeps `.env` from being required for the
one credential every user needs on day one.

**Ordering matters for failure cases.** Connectivity is tested before anything is
written, so a wrong password costs nothing. `oms.toml` is written before
provisioning, so a half-failed provision still leaves the generated secrets
recoverable — fix the cause and run `oms database init`. `oms init` refuses if
`oms.toml` already exists, matching `database init`'s strictness.

`oms init --non-interactive` takes the same values from flags or environment and
generates the rest, so the existing CI job keeps working unchanged.

**`oms init` wraps `oms database init`; it does not replace it.** The database verbs
(`init`, `migrate`, `drop`, `status`) keep their current behaviour and flags — CI
exercises them directly and they are proven. `oms init` is the first-run path that
generates `oms.toml` and then calls `database init` with the values it just wrote.
Two commands, one of which is a friendlier entry to the other.

**Configuration precedence becomes flag → env → `oms.toml` → default**, extending
the existing tiers in `setup/database/config.rs` rather than replacing them.

**Key rotation** ships in this project as `oms config rotate-key`: decrypt with the
old key, re-encrypt with the new, one transaction. Trivial with three rows and
awkward to retrofit later.

**`database status` drops its superuser requirement.** It reads `pg_roles`,
`pg_database` and `_mdm_migrations`; the first two are public catalogs, and the
migration grants `SELECT ON public._mdm_migrations TO oms`. Only `migrate` and
`drop` ever prompt.

### Storage

```sql
ALTER TABLE oms.broker_connection
  ADD COLUMN credentials             BYTEA,
  ADD COLUMN credentials_updated_at  TIMESTAMPTZ,
  ADD COLUMN credentials_updated_by  TEXT;   -- reserved for accounts

CREATE TABLE oms.feed_connection (
    code         TEXT PRIMARY KEY,           -- 'databento-opra'
    provider     TEXT NOT NULL,              -- DATABENTO
    dataset      TEXT,                       -- OPRA.PILLAR
    status       TEXT NOT NULL DEFAULT 'ACTIVE',
    credentials  BYTEA,
    credentials_updated_at TIMESTAMPTZ,
    credentials_updated_by TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE','DISABLED'))
);

GRANT SELECT ON public._mdm_migrations TO oms;
```

`credentials` is nullable: a connection may exist unconfigured, which is what the
cockpit shows as "needs setup".

### Encryption — `src/secrets.rs`

```rust
pub fn seal(master: &Key, aad: &str, plaintext: &[u8]) -> Vec<u8>;
pub fn open(master: &Key, aad: &str, sealed: &[u8]) -> Result<Vec<u8>, SecretError>;
```

AES-256-GCM via the `aes-gcm` crate — no new system dependency. Stored bytes are
`[12-byte nonce][ciphertext‖tag]`, the nonce fresh per write from the OS RNG.

**The connection's `code` is the AAD.** That binds each blob to its row, so a
`credentials` value copied onto a different connection fails to decrypt rather than
silently authenticating as the wrong account.

### Credentials

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum BrokerCredentials {
    Alpaca { key: String, secret: String },
    BinanceFix {
        host: String, port: u16,
        sender_comp_id: String, target_comp_id: String,
        private_key: String,          // PEM contents, not a path
    },
    IbkrFix {
        host: String, port: u16,
        sender_comp_id: String, target_comp_id: String,
        password: String,
        ssl: bool,
    },
}
```

The private key becomes **bytes in the database**, not a filesystem path. A path
into the operator's filesystem is precisely what makes the current setup
un-configurable from a GUI.

**Redaction is per-variant and handlers never return plaintext.** `Alpaca` returns
the key's last four characters and a null secret; FIX variants return host, port
and comp IDs in clear and omit the private key entirely. Editing means
re-submitting a field, never reading the old value back. A configuration UI that
hands plaintext API secrets to a browser is a mistake independent of how they are
stored.

### Live reload

**Brokers.** `AppState`'s `Arc<BrokerRegistry>` becomes `ArcSwap<BrokerRegistry>`:
writers build a new map with the changed entry and swap the pointer, readers do an
atomic load. Lock-free on the order path, which is hot; swaps are rare.

The safety property falls out for free — `registry.get()` returns an
`Arc<dyn BrokerAdapter>`, so an order already routing holds its own clone and
finishes against the adapter it started with. A credential change never yanks the
client out from under an in-flight order.

**Save order is decrypt → build adapter → test → persist → swap.** The test
precedes the write, so saving broken credentials fails the request and leaves both
the database and the running registry untouched. "Test connection" is the gate, not
a convenience.

**Feeds.** `supervise()` returns `!` (`stream_supervisor.rs:42`) and owns its
`Session`, which owns the credentials, so there is no graceful stop: restart is
`JoinHandle::abort()` plus a respawn with a freshly built session. A small
`StreamRegistry` keyed by stream name holds the handles.

Aborting mid-flight is acceptable *here specifically* because a quote feed already
assumes it will drop and reconnect — that is what the backoff exists for. Losing an
in-flight quote to an abort is indistinguishable from losing it to a network blip.

**Boot loads from the database**, decrypts, and registers through the same code
path a live save uses. One way an adapter comes into existence, not two.

**Disabling a connection removes it from the registry.** Orders routed to it fail
immediately with the existing "no adapter" error rather than queueing — a disabled
broker should reject, not silently accumulate.

### API

Extends the existing `/admin/broker-connections` routes (`main.rs:681-685`).

```
GET    /admin/broker-connections                      list, redacted
PUT    /admin/broker-connections/:code/credentials    set/replace; tests first
POST   /admin/broker-connections/:code/test           test what is stored
DELETE /admin/broker-connections/:code/credentials    clear
GET    /admin/feed-connections                        … same four for feeds
GET    /admin/setup-status                            the checklist
```

**Connection tests are per-adapter and differ in cost.** Alpaca is a
`GET /v2/account`, sub-second. Databento is an auth handshake. FIX has no cheap
test — the only real one is attempting a logon, so it runs with a timeout and takes
seconds. The UI shows a spinner and does not pretend all connections test alike.

```jsonc
// GET /admin/setup-status
{"database": "ok",
 "admin_password": "default",
 "brokers":  [{"code": "alpaca-paper", "state": "unconfigured"}],
 "feeds":    [{"code": "databento-opra", "state": "ok"}],
 "catalog":  {"instruments": 0, "state": "empty"},
 "portfolios": 0}
```

This single endpoint answers "what do I still have to do" — the question no part of
the system can currently answer, since today it is a warning in stdout.

### Cockpit

`BrokerConnections.tsx` and `DataFeeds.tsx` gain an edit drawer per row: a small
hardcoded form per broker (Alpaca two fields; Binance FIX six plus a file picker
for the private key), a **Test** button, and a **Save** that refuses on a failed
test. Configured credentials render as `••••` with "updated 3 days ago".

A **first-run setup view** renders `setup-status` as a checklist and stays
reachable afterwards. Its catalog item offers **Sync now**, which fires
`setup sync-broker` in the background; the UI polls `setup-status` until
`instruments > 0`. Crude, but it completes the wizard without inventing a job
system. A `sync_run` table becomes worthwhile when history or a "last synced"
timestamp is wanted — a natural follow-up, not a prerequisite.

## Migration

`oms config import-env` reads the current `ALPACA_*`, `BINANCE_*` and
`DATABENTO_*` variables, seals them into connection rows, and prints what it
imported. Run once; then delete those lines from `.env`.

## Failure modes

| Situation | Behaviour |
|---|---|
| Master key missing, credential rows exist | Refuse to start. Nothing decrypts; starting "successfully" with no brokers would be a lie. |
| Credentials fail to decrypt | Connection reports `credential_error`, adapter not registered, server starts. Visible in `setup-status`. Never silently treated as unconfigured — that invites re-entry and masks a key-management problem. |
| Test fails on save | 422 carrying the broker's own message. Nothing written, registry untouched. |
| Adapter builds, broker is down | Save succeeds; it tested moments ago. `stream_health` covers the ongoing story. |
| Two admins save the same connection | Last write wins; `credentials_updated_at` records when. Not worth locking for a small team. |

## Testing

The crypto and redaction layers are pure and get real unit coverage:

- seal/open round-trip
- tampered ciphertext rejected
- **AAD mismatch rejected** — the copy-a-blob-to-another-row case
- **per-variant: the redacted view contains no plaintext secret**

That last one is the regression most worth pinning: it is the mistake that leaks
keys to a browser.

The registry swap gets a unit test with a stub adapter — swap under a concurrent
reader, assert the reader's held `Arc` still works. Database-backed tests go behind
`#[ignore]`, matching `applying_twice_is_a_no_op`.

The live feed restart is deliberately not tested heavily: it is an `abort()` and a
respawn, and a meaningful test would need a fake exchange. Existing supervisor
tests cover the backoff logic that matters.

## Consequences

- **`oms.toml` becomes the most security-sensitive file in the deployment.** Losing
  it makes every stored credential unrecoverable. `oms init` says so, and the
  cockpit prompts for a backup.
- **Three commands can reach a database-destroying credential** — `init`, `migrate`,
  `drop` — and all three prompt for it rather than reading it from disk.
- **The environment stops being the source of truth for credentials.** Anyone
  debugging authentication looks in one place.
