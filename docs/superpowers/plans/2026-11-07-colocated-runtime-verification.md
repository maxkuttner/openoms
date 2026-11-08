# Co-located runtime verification

Date: 2026-11-08. Branch `feature/colo-runtime`, base `18f738b` plus Task 12 changes.
Host: macOS, aarch64, rustc 1.93.1.

## Commands and observed results

| Command | Result |
|---|---|
| `cargo test --workspace` | all targets passed; principal suites: `rustoms` 640 passed / 100 ignored, gateway 173, gateway no-FIX 146, colo-link 14, colo-runtime book 16 / control 10 / journal 11 / recovery 9 / reporter 2 / runner 5 / system 3, colo-example 5 |
| `cargo test -p gateway --no-default-features` | 146 passed, 0 failed |
| `cargo test -p colo-runtime --test system` | 4 passed: durable intent survives restart, persisted kill blocks admission after restart, blocking venue call cannot stall kill, post-start stream fill moves position/leaves |
| `COLO_TEST_DATABASE_URL=… scripts/test-colo-isolated.sh` against a disposable `postgres:16` container on a private port | system 4, deployment 7, ingest 2, risk-engine 2 — all passed; migrations `0041`/`0042` applied |
| `cargo test -p colo-example` | 5 passed |
| `cargo run --release -p colo-runtime --example latency -- 2000` | `p50 18,125 ns`, `p99 35,583 ns`, `p999 37,625 ns`, max `110,250 ns`, 2,000 samples, 0 errors |
| `cargo test -p gateway --no-default-features connectors::binance::runtime::tests::testnet_read_only_smoke -- --ignored` | passed: 1,373 spot instruments, 517 balances, 0 open orders from Binance Spot testnet; no order placed/cancelled |
| `npm run check` in `docs-site` | 38 tests passed, 0 failed |
| `git diff --check` | clean |

The latency number is local admission/reservation work only. It excludes journal
fsync, venue/network latency, and OMS reporting. Do not present it as an
end-to-end order latency.

## Isolated database suite

`scripts/test-colo-isolated.sh` exists and requires an explicit
`COLO_TEST_DATABASE_URL` whose database name contains `test`, `colo` or
`scratch`. It sets `OMS_CONFIG=/nonexistent`, never loads `oms.toml`, and runs
only named co-located ignored tests plus the local system test.

This suite was run against a fresh, disposable `postgres:16` container on a
private loopback port, with its own database (`ods_colo_test`) created through
`oms database init`. The shared `oms-postgres` container and `oms-pgdata` volume
were not touched.

Verified there:

- runtime migrations `0041`/`0042` apply cleanly;
- duplicate account registration is refused;
- policy snapshot publication advances revisions;
- epoch-exact kill/enable commands persist across reload;
- unverified ownership replacement is refused;
- inbox replay is idempotent, changed payload suspends, sequence gaps are rejected;
- one economic trade deduplicates and a later fee enriches the native-asset ledger;
- directional exposure and concurrent admission tests pass.

## Live/TLS gates still open

- A local TLS two-socket fixture now runs in `link_server` tests: bootstrap over
  `/v2/control`, a bounded batch over `/v2/events`, durable accept, and ACK back
  over control. Real OMS/Postgres binding still needs the isolated DB harness.
- Binance Spot testnet was reached read-only: signed account and open-orders
  requests plus public exchangeInfo passed. Order placement/cancellation,
  user-data streams, recovery and fee-asset reconciliation remain unverified
  and require explicit approval for side-effecting testnet orders.
- The reference example still uses the v1 control link with the shared `risk`
  evaluator; it has not yet been migrated to the embedded runtime. This is an
  explicit open gate, not a completed feature.
- Journal fsync cost is synchronous by design and is not included in the latency
  sample above. Measure it before claiming a production latency target.

## Shipped vs unverified

Implemented and unit-tested: shared risk evaluator, Binance runtime recovery
contract, deterministic Book/asset reservations, durable journal, v2 protocol,
deployment registration/bootstrap/admin endpoints, recovery barrier/control
state, durable OMS inbox/economic trade identity/native-asset ledger, runtime
runner with post-start event ingestion, policy/allocation application, an
injected link transport, and a replay reporter connected through that link.

Unverified until the remaining gates run: venue/testnet behaviour, end-to-end
journal-inclusive latency, and reference-program runtime migration. The runtime
deliberately does not build sockets itself: the caller injects an authenticated
`RuntimeLink`; `V2RuntimeLink` is the production adapter. Both isolated DB and
local TLS two-socket fixtures pass.
