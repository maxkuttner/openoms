# Live credential reload — final review and verification

Date: 2026-09-13
Plan: `docs/superpowers/plans/2026-09-13-live-credential-reload.md`
Original implementation reviewed: `a093868..4e9c04b` (nine commits).

## Verdict

Plan 3 is ready for local merge after the fixes and verification below. All six
original tasks had completed individual reviews. This review closes the interrupted
whole-branch review and the missing HTTP/store integration check.

## Findings resolved

1. **Concurrent reloads could interleave publication and stream replacement.**
   An application-wide async reload mutex now covers store reads, registry building,
   publication and stream reconciliation. Order readers remain lock-free.
2. **`JoinHandle::abort()` did not prove termination before respawn.**
   `StreamRegistry::abort_and_remove` now joins the aborted task without holding its
   map mutex. Both live restart functions await it before spawning the replacement.
3. **Adding shutdown awaits made HTTP cancellation a partial-update hazard.**
   The reload runs in an owned Tokio task. Dropping the request's future does not
   cancel a reload while it is replacing streams.
4. **Binance REST disable removed its adapter but left its execution stream alive.**
   Startup now registers those handles; reload stops them when no corresponding
   Binance adapter survives. Credential changes still require restart for Binance.
5. **Deleted rows had no loop iteration, leaving old tasks alive.**
   Known execution/feed tasks are reconciled against the resulting registry and
   feed set, including absent rows. Stopping Databento removes its doorbell sender.
6. **Independent store queries could straddle a key rotation.**
   Both tables are read in one repeatable-read, read-only transaction. Store loaders
   accept an SQLx executor so startup can retain its pool calls while reload uses
   the transaction connection.
7. **Stopped/restarting tasks could retain stale `Live` health.**
   Joined stops mark existing tracked streams down; new sessions reset to connecting.
8. **Documentation overstated what registration proves.**
   The README distinguishes installation from remote authentication and documents
   serialized reloads, reconnection gaps and master-key restart requirements.

## Verification executed

| Check | Result |
|---|---|
| `cargo build` | Passed; 20 existing warnings |
| `cargo test` | 266 passed, 0 failed, 3 ignored |
| `cargo clippy --all-targets` | Passed; 34 warnings per binary/test target, none in changed reload code or tests |
| `git diff --check` | Passed |
| Disposable Postgres 16 `database init` | Passed; 46 migrations, access policy, reference data |
| All three ignored database tests, run explicitly | 3 passed, 0 failed |

The database checks used container `oms-reload-verification`, bound only to
`127.0.0.1:55443`, with synthetic credentials. Commands ran with an empty inherited
environment and a scratch working directory, using the compiled binary/test
executable directly. Neither the repository's `.env` nor `oms.toml` was loaded.
The container was removed after verification.

Database tests exercised:

- Existing credential save/load/key-rotation round trip.
- Existing migration idempotence check.
- New `reload_http_lifecycle_against_postgres`: a fresh private database with the
  real migrations, real encryption/store queries, real HTTP middleware and reload
  orchestration; only external session startup is replaced by controlled tasks.

The HTTP checks cover missing/wrong admin tokens; store failure preserving state;
empty-store success; registered Alpaca/Databento outcomes; exact FIX adapter
carry-forward; repeated reloads with one task per stream; adapter/stream pairing;
concurrent requests blocked during replacement; caller cancellation; partial and
total decryption failure; disable/clear/removal; stopped-stream health; and response
redaction. The stream-registry unit test now checks immediate channel disconnection
after shutdown returns, rather than waiting for cancellation at a later await.

The new ignored test uses `OMS_RELOAD_TEST_DATABASE_URL` explicitly, creates a
unique database owned by the already-provisioned `oms` role, and drops it at the
end. CI supplies this variable in its existing disposable-Postgres job.

## Remaining scope and limitations

- No real Alpaca, Databento, Binance or FIX authentication was attempted. These
  checks verify local lifecycle and orchestration, not exchange-side behavior.
- `Registered` is not a connectivity test. Test-before-save belongs to Plan 4.
- Registry publication is atomic; a broker-side credential cutover is not.
  Reconnection has a gap. Existing Alpaca reconciliation remains responsible for
  recovering missed executions; this review does not establish zero-loss delivery.
- FIX sessions cannot be stopped/reloaded. Disable removes order routing, but the
  underlying session continues until process restart. Binance REST credential
  changes also retain their conservative restart-required behavior.
- Changing the master key requires process restart because file configuration is
  memoized. Reload refuses when all stored credentials are unreadable.
- The existing credential-kind versus broker-code mismatch is still only warned
  about during registration. Plan 4 must reject mismatched credential writes.
- The previous deferred concurrency, dead-doorbell and missing HTTP integration
  items are resolved. Dependency-level ArcSwap concurrency tests and unrelated
  warning cleanup remain unnecessary for this feature.

## Next

Merge Plan 3 locally, then execute the separate Plan 4 preparation: credential
write/test/clear API, redacted status, cockpit forms, and first-run setup workflow.
