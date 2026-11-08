# Co-located runtime verification

Date: 2026-11-08. Branch `feature/colo-runtime`, base `18f738b` plus Task 12 changes.
Host: macOS, aarch64, rustc 1.93.1.

## Commands and observed results

| Command | Result |
|---|---|
| `cargo test --workspace` | all targets passed; principal suites: `rustoms` 640 passed / 100 ignored, gateway 173, gateway no-FIX 146, colo-link 14, colo-runtime book 16 / control 10 / journal 11 / recovery 9 / reporter 2 / runner 5 / system 3, colo-example 5 |
| `cargo test -p gateway --no-default-features` | 146 passed, 0 failed |
| `cargo test -p colo-runtime --test system` | 3 passed: durable intent survives restart, persisted kill blocks admission after restart, blocking venue call cannot stall kill |
| `cargo test -p colo-example` | 5 passed |
| `cargo run --release -p colo-runtime --example latency -- 2000` | `p50 18,125 ns`, `p99 35,583 ns`, `p999 37,625 ns`, max `110,250 ns`, 2,000 samples, 0 errors |
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

**Not run here:** no disposable, migrated Postgres URL was provided. Therefore
the following remain unverified in this environment:

- runtime migration `0041`/`0042` application against a real Postgres;
- duplicate account-alias registration;
- deployment policy revision publication;
- epoch-exact kill/enable persistence;
- inbox sequence gap, replay conflict and durable ACK;
- one-economic-trade deduplication and later fee enrichment;
- risk-engine directional exposure ignored tests.

## Live/TLS gates still open

- No local TLS v2 control/event system fixture was run in this pass. The pure
  protocol/state tests run, and the OMS event socket now durably accepts batches
  and pushes ACKs, but two-socket reconnect/OMS binding still needs the isolated
  system harness.
- No Binance testnet or live venue traffic was used. Venue-specific recovery,
  cancellation and fee-asset reconciliation remain unverified.
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

Unverified until the gates above run: production migration, isolated database
behaviour, local TLS two-socket flow, venue/testnet behaviour, end-to-end
journal-inclusive latency, and reference-program runtime migration. The
runtime deliberately does not build sockets itself: the caller injects an
authenticated `RuntimeLink`; `V2RuntimeLink` is the production adapter. That
real two-socket path still needs the local TLS/OMS fixture.
