# Embedded Co-located Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give trusted co-located strategies a reusable, recoverable in-process execution runtime with local risk reservations and asynchronous OMS supervision/accounting.

**Architecture:** Share a pure `risk` evaluator between OMS admission and an embedded `colo-runtime`. The runtime owns immediate execution exposure, configuration, journal and recovery; `gateway` owns venue protocol; `colo-link` carries versioned control and replayable events. Preserve path ① transactional commit-before-send and FIX/REST interfaces.

**Tech Stack:** Rust 2021, Tokio, serde/JSON, existing TLS WebSocket infrastructure, SQLx/Postgres on the OMS side, Binance Spot adapter, Astro documentation.

**Spec:** `docs/superpowers/specs/2026-11-07-colocated-runtime-design.md`

## Global Constraints

- Trusted Rust strategies, Binance Spot capability coverage, one active runtime per dedicated venue account/subaccount, one strategy allocation.
- No per-order remote OMS/database dependency. No extra execution process or internal REST hop.
- Retain durable-before-send local journal contract. Asynchronous journaling is a separate design change, not a hidden optimization. Do not add a selectable weak mode in this implementation.
- Path ① retains transactional admission and commit-before-send; common rules do not imply common storage.
- Use independent venue observation plus at-least-once runtime delivery, with one durable accounting authority and economic-fill deduplication.
- Suspended, expired-authority and killed runtimes permit cancellation/recovery only. Reducing needs Ready, valid authority and explicit policy.
- Secrets are local restricted references, never bootstrap payloads, event bodies or logs. Never read/print `oms.toml` or test keys.
- Never run ignored DB tests against the local/shared development database; never touch `oms-postgres` or `oms-pgdata`. Disposable test database only, isolated worktree, no `.env`/`oms.toml`, explicit `POSTGRES_*` and `OMS_CONFIG=/nonexistent`.
- No commits or pushes unless explicitly requested. Review task-sized diffs after each green cycle; if commits are requested later, stage exact intended paths. Preserve unrelated files listed in HANDOFF.md.
- Target documentation remains labeled not implemented until corresponding implementation and verification gates pass. No live order or real account migration in automated verification.

## Review Focus

1. Duplicate fills from runtime and venue-follow, in either arrival order, must book once including fees; test Task 9.
2. Crash after venue send but before reply, including unknown orders absent from open-order query, must block reuse of exposure; test Tasks 5 and 8.
3. Shared USDT balance across BTC/USDT and ETH/USDT must not be double-spent, and venue locked balances must not be double-reserved; test Task 4.
4. Heartbeat/event backlog or slow disk/network must not renew stale authority or prevent admission from stopping; test Tasks 6 and 10.
5. Legacy booked trades and process restarts must not create new accounting identities or silently clear kill; test Tasks 7 and 9.

## Work structure and dependency order

Four milestones, one plan: shared semantics (1–2); execution kernel (3–5);
control/recovery/accounting (6–9); integration and rollout evidence (10–12).
Execute sequentially unless an approved execution method explicitly delegates
independent tasks. Each task must leave its tests green; intermediate crates need
not enable a partially safe production runtime.

### File responsibilities

| Files | Responsibility |
|---|---|
| `crates/risk/{Cargo.toml,src/lib.rs,src/types.rs,src/evaluate.rs,src/tests.rs}` | Shared evaluator and explicit unit/state types |
| `src/domain/orders/risk.rs`, `src/risk_engine.rs`, `src/strategies/guard.rs` | OMS adapters and coherent transactional exposure |
| `crates/gateway/src/runtime.rs`, `connectors/binance/runtime.rs` | Complete runtime venue capabilities, recovery and event contract |
| `crates/colo-runtime/src/{lib,book,assets,journal,recovery,control,runner,reporter}.rs` | Embedded runtime split by state ownership and I/O responsibility |
| `crates/colo-link/src/{proto,v2,client}.rs` | Existing v1 plus explicitly negotiated v2 control/report protocol |
| `src/colo/{deployment,bootstrap,ingest}.rs` | Durable ownership, consistent bootstrap and replay inbox |
| `src/colo/{link_server,control,follow,booking,mod}.rs` | Existing server/control/independent follow integration |
| `src/strategies/{admin,control,mod}.rs`, `src/admin.rs`, `src/main.rs` | Registration/control API integration; `ApiDoc` and route registration live in `src/main.rs` |
| `db/migrations/ods/oms/0041_CREATE_COLO_DEPLOYMENT.sql`, `0042_CREATE_COLO_EVENT_INBOX.sql` | Deployment/command ownership, replay and economic-fill identity |
| `crates/colo-example/src/{main,quote}.rs` | Strategy-only example consuming runtime |
| Architecture component/page and `strategies/colocated.md` | Shipped/target distinction, migration and operational guide |

The repository currently ends at migration 0040. Confirm numbering immediately
before adding migrations. Do not modify already applied migration files.

## Task 1: Shared, validated risk evaluator

**Files:** Create `crates/risk/Cargo.toml`, `src/{lib,types,evaluate,tests}.rs` under that crate. Modify root `Cargo.toml`, `Cargo.lock`, `crates/colo-risk/{Cargo.toml,src/lib.rs}`, `crates/colo-link/Cargo.toml`, `crates/colo-example/Cargo.toml` as needed for temporary compatibility.

**Interfaces:** In `risk`, expose `Side::{Buy,Sell}`, `TradingState::{Active,Reducing,Halted}`, `Limits`, `Exposure { position_qty, working_buy_qty, working_sell_qty }`, `OrderInput { side, quantity, limit_price, multiplier }`, `Valuation { order_price: Option<f64>, position_price: Option<f64>, reference_price: Option<f64> }`, `RiskInput { order, exposure, limits, state, valuation }`, and `Reject { code: String, message: String }`. Numeric fields use validated finite `f64` consistent with existing domain; limit absence remains `Option`. `evaluate(&RiskInput) -> Result<(), Reject>` is pure. `resolve_limits(defaults: &Limits, overrides: &Limits, quote_currency: &str) -> Result<Limits, Reject>` defines existing per-field precedence and explicit currency behavior. Rate state is not part of the evaluator.

- [ ] Write failing tests in `crates/risk/src/tests.rs`: `opposing_working_orders_do_not_offset` (position 0, buys 100, sells 100, candidate buy 100, cap 100 rejects); `reducing_cannot_cross_flat` (long 10, sell 11 rejects); `reducing_reserves_remaining_capacity` (long 10, working sells 6, new sell 5 rejects); `multiplier_applies_once` (2 × 50 × 100 = 10,000); `invalid_inputs_rejected` (NaN, infinity, negative quantity, zero reference); `position_valuation_not_candidate_bid` (position risk uses supplied valuation, not low order price).
- [ ] Run `cargo test -p risk`; expect missing package/API or assertion failure before implementation.
- [ ] Implement types/evaluator and precedence, retaining existing rejection codes where semantics match. Separate order notional, position valuation and price-band inputs. Preserve existing ordinary-OMS behavior when no valuation is provided through an explicit caller decision, not a hidden price fallback. Reject invalid configured limits and overflowing calculations. Move common checks out of `colo-risk`; retain temporary legacy adapter for old example until Task 11, clearly not a reservation runtime.
- [ ] Run `cargo test -p risk` and `cargo test -p colo-risk`; require all applicable cases green. Review diff for unintended default-policy changes; do not claim v1 example is hardened by extraction.

## Task 2: Integrate shared rules without weakening path ①

**Files:** Modify `src/domain/orders/risk.rs`, `src/risk_engine.rs`, `src/strategies/{guard,mod}.rs`, `src/order_entry.rs`, root manifest; tests in these modules plus existing strategy order-path tests.

**Interfaces:** Keep root `RiskRejection`, domain `OrderSide` and existing HTTP/FIX errors as adapters to `risk`. Change `RiskDataProvider::exposure` to supply directional exposure. `guard::check_state` must receive order quantity and full exposure, not side/position alone. Root `Strategy::risk_limits` returns shared limits (v1 serializer adapts field names).

- [ ] Write failing unit tests for shared evaluator use in ordinary and strategy admission, contract multiplier, per-instrument override and missing-price compatibility. Add ignored isolated-DB tests `directional_exposure_reads_one_snapshot` and `concurrent_submits_share_capacity`: racing buys against cap admit at most permitted capacity; moving quantity from working to position cannot make it disappear between reads.
- [ ] Run `cargo test -p rustoms risk` and `cargo test -p rustoms strategies::guard`; expect failures on new semantics. Database cases run only through isolated harness in Task 12.
- [ ] Replace separate arithmetic/state/band checks with shared evaluator adapters. Read position and two directional working sums in one SQL statement under existing scope admission locks. Retain insertion and commit before venue send. Maintain no-limit behavior for ordinary OMS orders, explicit currency overrides and existing FIX/REST response shape. Do not silently introduce colo rate caps or Spot asset ledger into ordinary OMS path.
- [ ] Revalidate mark age at admission after lock wait; where a configured check requires a price, reject stale supplied marks rather than using an aged pre-lock result. Keep documented uncovered-instrument/no-price compatibility separate from stale supplied data and record intentional changes in docs/changelog.
- [ ] Run focused tests and `cargo test -p rustoms`; inspect every former `check_limit`, `check_trading_state` and price-band caller. Keep regression tests; update only assertions representing documented deliberate stricter semantics.

## Task 3: Venue runtime contract and complete Binance reports

**Files:** Create `crates/gateway/src/runtime.rs`, `crates/gateway/src/connectors/binance/runtime.rs`; modify `gateway/src/lib.rs`, Binance `mod.rs`, `adapter.rs`, `stream.rs` and `contract.rs`. Add fixture unit tests beside parsers.

**Interfaces:** Add `RuntimeVenue: BrokerAdapter` with async `metadata(symbols: &[String]) -> Result<Vec<InstrumentRules>, BrokerError>`, `balances() -> Result<Vec<AssetBalance>, BrokerError>`, `snapshot_orders() -> Result<Vec<RuntimeOrder>, BrokerError>`, `executions(cursor: &ExecutionCursor) -> Result<ExecutionPage, BrokerError>`, `lookup(client_id: &str, symbol: &str) -> Result<RuntimeOrder, BrokerError>`, and `subscribe(tx: mpsc::Sender<VenueEvent>) -> Result<(), BrokerError>`. Define serializable `InstrumentRules`, `AssetBalance`, `RuntimeOrder`, `ExecutionCursor`, `ExecutionPage`, `VenueEvent`, `TradeKey`, `Trade` here, without depending on OMS or `colo-runtime`.

Define shared `AccountIdentity { account_id: Uuid, venue: String, environment:
String, external_account_ref: String }` and `RuntimeEvent` in gateway's runtime
contract so link/reporting and local reducer share types without a dependency cycle.
`RuntimeEvent` variants cover admitted intent, submit outcome, venue event, health,
policy/control receipt and recovery checkpoint; all carry stable order/account
identity where applicable. Runtime-owned timing/state types remain in runtime.

`TradeKey` includes venue, account identity, environment, symbol and trade ID.
`Trade` includes key, order/client IDs, side, quantity, price, original timestamp,
optional fee amount/currency and provenance. Unknown fee differs from zero.
`RuntimeOrder` includes original/leaves/executed quantity, side, limit price, state,
symbol and both IDs. `ExecutionPage` includes next cursor and explicit completeness.
`InstrumentRules` includes canonical identity, base/quote, tick/step, multiplier,
min/max quantity/notional and supported operations. Capabilities declare unsupported
cancel-on-disconnect and missing recovery features rather than imply support.

- [ ] Write failing fixture tests for all lifecycle outcomes, partial-fill remaining quantity, original time/fees, same trade ID on two symbols/accounts, pagination beyond 1,000 trades, unknown client-ID lookup, market precision filters and stale/closed stream health. Test timeout/5xx remain unknown, not definitive rejection.
- [ ] Run `cargo test -p gateway --no-default-features runtime`; expect missing contract/parser assertions.
- [ ] Implement Binance Spot runtime contract using existing signing and transport. Share normalized trade parsing between runtime and follow path; preserve v1 adapter interfaces through mapping. Recovery covers explicit instrument universe plus journal/discovered historical symbols, not only open orders. Bound queries and report missing history as incomplete. Do not fabricate an account identity if endpoint cannot establish it; carry operator-verified identity requirement into bootstrap.
- [ ] Run `cargo test -p gateway --no-default-features` and workspace compilation. Parser tests use fixed payloads, not network credentials. Inventory units and fees in third assets must survive parsing.

## Task 4: Deterministic reservation and execution reducer

**Files:** Create `crates/colo-runtime/{Cargo.toml,src/lib.rs,src/book.rs,src/assets.rs}` and `tests/book.rs`; add workspace membership. Depend on shared `risk` and gateway contract, not OMS/SQLx.

**Interfaces:** `Book::new(identity: AccountIdentity, limits: BTreeMap<String, risk::Limits>) -> Result<Book, RuntimeError>`; `Book::prepare(intent: Intent, prices: &Prices, now: Instant) -> Result<PreparedOrder, RuntimeError>`; `Book::apply(event: &VenueEvent) -> Result<Vec<RuntimeEvent>, RuntimeError>`; `Book::request_cancel(id: &str) -> Result<CancelRequest, RuntimeError>`. `Intent` carries instrument, side, quantity, limit/bounded market spend and TIF. `PreparedOrder` carries stable client ID, broker request, reservation and journal record; only runner can send it. `AssetLedger` tracks total/free/committed base and quote assets. Public `RuntimeError` distinguishes rejection, unavailable authority, incomplete recovery and corrupt state.

`Book::new` validates identity and limits. `Prices` maps canonical instruments to order/reference/position valuation
plus monotonic receipt times. `Submission` identifies local acceptance and client
order ID, never promises venue acknowledgement. Use Task 3's `AccountIdentity` and
`RuntimeEvent` throughout; do not define alternate protocol copies. Task 4's
prepared record is pure data; Task 5 wraps it in durable `JournalRecord`.

- [ ] Write failing tests: cap 10 admits first buy 6 and rejects second; fill 4 of buy 10 yields position 4/leaves 6 in one transition; duplicate fill leaves both unchanged; cancel request/timeout retain reservation; definitive reject releases; late fill after cancel still changes position once; replacement charges overlap; reducing capacity cannot cross flat.
- [ ] Add `two_symbols_share_quote_balance`: 1,000 USDT, BTC buy reserves 600, ETH buy needing 600 rejects; fee reserve prevents spending last funds; free+locked venue snapshot does not charge same order twice. Reject unbounded market spend and precision violations; do not round risk-increasing amounts down.
- [ ] Run `cargo test -p colo-runtime --test book`; expect missing API/failed assertions.
- [ ] Implement deterministic reducer and asset reservation ownership. Per-account rate window counts each accepted new/replace admission and each separately submitted retry; risk rejections do not count, cancels use separate venue throttle. Unknown sends are not retried blindly. Unknown external orders pause admission and require recovery. Reuse shared evaluator; no venue or DB calls in `Book`.
- [ ] Run reducer and shared-risk tests. Add two concurrent callers through a serialized test owner and verify sum of accepted commitments respects cap. Record rounding policy explicitly, with conservative comparison and tick/lot boundary tests.

## Task 5: Recoverable local journal and stable identifiers

**Files:** Create `crates/colo-runtime/src/journal.rs`, `tests/journal.rs`; extend error/event types and manifest only for existing workspace libraries as needed.

**Interfaces:** `Journal::open(path: &Path, identity: &AccountIdentity) -> Result<Journal, RuntimeError>`; `append_durable(record: &JournalRecord) -> Result<u64, RuntimeError>`; `replay() -> Result<Vec<SequencedRecord>, RuntimeError>`; `ack_through(sequence: u64) -> Result<(), RuntimeError>`; `unacked(from: u64, limit: usize) -> Result<Vec<SequencedRecord>, RuntimeError>`. Records include deployment identity, epoch, persistent monotonic sequence and schema version. `JournalRecord` variants cover intent/reservation, venue event, installed policy identity, kill/enable and durable OMS ACK. Client ID uses bounded Binance-compatible UUID representation; retain deployment/sequence mapping in journal instead of concatenating unbounded strings.

- [ ] Write failing temporary-directory tests for torn final append, checksum failure in middle, mismatched account/identity, restart after intent before send, crash after send before response, ACK loss/replay, unsupported schema, full disk/write failure and compaction retaining live/unknown orders plus kill latch.
- [ ] Run `cargo test -p colo-runtime --test journal`; expect missing implementation/failing cases.
- [ ] Implement length-delimited checksummed records with schema/header and explicit flush+sync before returning durable success. Truncated uncommitted tail is recoverable; midstream corruption blocks admission. Use exclusive local ownership (supported locking primitive) and release on process exit; do not silently accept second writer. Durable ACK never exceeds contiguous stored events. Atomic compaction keeps checkpoint and unacknowledged tail; persist parent-directory changes when needed for crash safety.
- [ ] Test with injected writer failures, not actual disk exhaustion. Verify no broker send occurs when intent durability fails. Journal I/O must run outside reducer/watchdog executor; final send authorization will be rechecked after journal completion in Task 10.

## Task 6: Explicitly versioned control and reporting protocol

**Files:** Create `crates/colo-link/src/v2.rs`, modify `lib.rs`, `client.rs`, manifest; add `tests/v2.rs`. Preserve v1 protocol until migration ends.

**Interfaces:** v2 envelopes contain `protocol_version = 2`, deployment/account identity, epoch, policy revision and sequence as appropriate. `Bootstrap` contains instrument rules, effective policy, timings, baseline/checkpoint, bounded allocation and recovery universe. `ToRuntime` variants: Bootstrap, Policy, Authority, Kill, Enable, EventAck, ResyncRequired. `FromRuntime`: Hello, PolicyAck, ControlAck, Heartbeat, Events, RecoveryStatus. Define `ControlAckStage::{Received,AdmissionStopped,CancelsSent,ConfirmedClear}`. `Received<T> { message: T, received_at: Instant }` timestamps on socket reception. Disk/JSON serialization excludes `Instant`.

Use two authenticated persistent WSS connections for v2: `/v2/control` for
bootstrap/commands/authority, `/v2/events` for bounded event batches and durable
ACKs. Same deployment and epoch bind both; event stream cannot authorize trading.
This avoids TCP event backlog delaying kill. v1 connection remains explicit legacy
mode; no automatic downgrade from v2 failure. Connection/session nonces prevent
old buffered authority messages becoming fresh authority after reconnect.

- [ ] Write failing serde/client tests: version rejection, wrong account, exact ACK, revision rollback, gaps, duplicate idempotent snapshot, old-session authority, stopped epoch enable mismatch, bounded frame size and reconnect without losing pending events. Delay event writes and verify control delivery is independent.
- [ ] Run `cargo test -p colo-link`; expect new tests fail, legacy ones remain valid.
- [ ] Implement versioned envelopes/clients and configurable required timings. Retain 1-second heartbeat interval; require explicit positive authority duration and market-data age at registration rather than invent production defaults. Reject authority shorter than heartbeat interval. Receipt-time validity includes expiry while awaiting processing; heartbeats alone cannot renew authority. Limit event frames to 256 KiB / 128 records; bound queues and request replay rather than allocate indefinitely. TLS/handshake and authorization have bounded timeouts (5 seconds), with no token logging.
- [ ] Run protocol tests with paused clock and local TLS fixtures; no public network access. Verify v1 State does not deserialize as v2 Bootstrap and cannot activate v2 runtime.

## Task 7: Durable deployment registration, bootstrap and kill ownership

**Files:** Create migration `0041_CREATE_COLO_DEPLOYMENT.sql`, `src/colo/{deployment,bootstrap}.rs`; modify `src/colo/{mod,link_server,control}.rs`, `src/strategies/{admin,control}.rs`, `src/admin.rs`, `src/main.rs` (`ApiDoc` and routes), and `docs/openapi.json`.

**Interfaces:** `register_deployment(pool: &PgPool, input: &DeploymentRegistration) -> Result<Deployment, DeploymentError>`; `bootstrap(state: &AppState, deployment: &Deployment) -> Result<colo_link::v2::Bootstrap, BootstrapError>`; `acquire_session(pool: &PgPool, deployment_id: Uuid, session_nonce: Uuid) -> Result<AuthorityGrant, DeploymentError>`; `apply_command(pool: &PgPool, command: &ControlCommand) -> Result<CommandReceipt, DeploymentError>`. Bootstrap API returns Result: query error never becomes empty positions.

Schema owns unique strategy/account/venue/environment deployment assignment,
operator-verified external account identity, epoch, installed/issued policy,
timings, persistent kill/enable command identity and explicit opening inventory
baseline. Account identity uniqueness must prevent duplicate physical-account
aliases where known. Registration/retirement uses authenticated admin endpoints;
extend API docs in same task. Retirement requires verified shutdown/fencing;
disconnect or lease expiry alone never grants new deployment ownership.

Expose admin-only POST/GET `/admin/strategies/{code}/deployments`, GET
`/admin/strategies/{code}/deployments/{id}`, and POST
`/admin/strategies/{code}/deployments/{id}/retire`. POST requires the account
identity, allowed instrument universe, positive authority/feed-age settings,
allocation, per-asset opening holdings and per-instrument quantity/cost baseline.
Snapshot generation rejects a baseline inconsistent with existing OMS positions;
new opening inventory is recorded as explicit audited baseline, never fake fills.
Balance-to-instrument attribution is explicit: do not infer the same base holding
as a separate position in every pair. Retirement records verified shutdown/fencing
evidence; changing allocation cannot reuse capacity of unresolved old commitments.

- [ ] Write failing isolated-DB tests for duplicate account alias, unauthorized strategy, wrong environment, missing metadata/precision, missing baseline, coherent policy snapshot, stale epoch, replacement while former owner unverified and kill persistence across server restart. Unit-test exact command/epoch acknowledgement handling.
- [ ] Run unit tests with `cargo test -p rustoms colo`; execute ignored cases only through isolated harness.
- [ ] Implement registration and snapshot in a consistent transaction. Include existing instrument overrides and instrument trading state, not only strategy defaults. Policy edits (including per-instrument admin edits and connection/universe changes) must publish a new complete snapshot or stop renewal until resync. Before renewal, compare issued policy with durable current policy; cached registry must not renew stale authority indefinitely. Update all admin write paths found by tracing risk-limit mutation, not only `changes_limits`.
- [ ] Reuse bearer authentication/address restrictions and bind both v2 connections to authorized deployment. Persist kill before ACK; ordinary Active updates cannot enable a killed runtime. Existing admin Enable emits explicit durable command referencing prior stop and epoch. Reconnect same deployment enters recovery; require verified stop for ownership transfer. Provide independent OMS account cancellation, never claim this is venue fencing.
- [ ] Run unit tests and generate OpenAPI through existing command using isolated/no-secret configuration. Validate schema contains new endpoints and explicit baseline/timing requirements. Path ① FIX/REST payloads remain unchanged.

Use `cargo run -- openapi` to obtain schema through the existing deterministic
generator; refresh `docs/openapi.json` using permitted file-edit tooling and run
the repository's OpenAPI freshness test. Never load development credentials to
generate documentation.

## Task 8: Recovery barrier and local policy authority

**Files:** Create `crates/colo-runtime/src/{recovery,control}.rs`, `tests/recovery.rs`, `tests/control.rs`.

**Interfaces:** `recover(venue: &dyn RuntimeVenue, journal: &mut Journal, bootstrap: &Bootstrap) -> Result<RecoveredBook, RuntimeError>`; `ControlState::apply(message: Received<ToRuntime>) -> Result<Vec<ControlAction>, RuntimeError>`; `ControlState::may_submit(now: Instant) -> Result<(), RuntimeError>`. States are Disconnected, Bootstrapping, Recovering, Ready, Suspended; persistent kill latch is separate from transient connectivity. `RecoveredBook` contains Book, execution cursors and evidence needed to acknowledge recovery, not an unconditional Ready token.

- [ ] Write failing fake-venue tests: restart with open/unknown orders; fill occurring between stream establishment and snapshot; duplicated history/stream fills; fully closed trades on configured symbol with no open orders; >24-hour outage with available history; incomplete history; stale OMS snapshot; empty response vs failed query; wrong inventory and fee asset; expired/replayed authority; conflicting second process; kill followed by Active snapshot must stay stopped.
- [ ] Run `cargo test -p colo-runtime --test recovery` and `cargo test -p colo-runtime --test control`; expect failures.
- [ ] Establish stream before reconciliation, buffer/reduce overlapping events by stable identities, paginate complete history from persisted per-symbol cursors, resolve every pending client ID and verify open orders/balances. Binance snapshots do not share an atomic global sequence: repeat reconciliation until stable within bounded attempts and reject ambiguous coverage. Missing history is a blocking error, never inferred flat. Distinguish venue total holdings from strategy net position baseline.
- [ ] Validate whole policy snapshot and swap atomically at admission boundary. Any reduction in limits triggers conservative cancellation of affected working orders; keep reservations until confirmed. On expiry, stale required market data, stream loss or unresolved state: stop submissions, cancel, retain commitments. Only explicit current Enable plus successful recovery clears persisted kill. Transport heartbeat must not conceal failed execution stream.
- [ ] Run all runtime tests with deterministic clocks and fake venue. Verify cancellation remains possible in every suspended state and no new reduce-only submission is admitted while authority expired.

## Task 9: Durable OMS inbox and one economic-fill booking identity

**Files:** Create `0042_CREATE_COLO_EVENT_INBOX.sql`, `src/colo/ingest.rs`; modify `src/colo/{booking,follow,mod}.rs`, `src/order_engine.rs`, `src/execution.rs`, `src/handlers.rs` and domain/event fee metadata only where actual persistence requires it. Tests beside modules.

**Interfaces:** `accept_batch(pool: &PgPool, deployment: &Deployment, batch: &EventBatch) -> Result<DurableAck, IngestError>` stores contiguous events and highest accepted sequence atomically. `drain_inbox(state: &AppState, deployment_id: Uuid) -> Result<DrainProgress, IngestError>` retries unprocessed records. `observe_trade(state: &AppState, identity: &AccountIdentity, trade: &Trade) -> Result<TradeReceipt, IngestError>` is common to runtime and independent follow. ACK means durable acceptance, not merely enqueueing to in-memory order engine.

Persist unique `(deployment, sequence)` inbox plus unique economic key
`(venue, account identity, environment, symbol, trade_id)`, mapped to existing OMS
order/fill identity and processing state. Acknowledged pending records survive
OMS crash. Conflicting duplicate payloads alarm and suspend affected deployment;
identical duplicates are idempotent. Fees enrich same trade once; unknown fees
must not prevent later enrichment or create another position movement.

Migration 0042 also creates append-only `colo_asset_event` keyed by economic
trade identity, asset and entry kind (base movement, quote movement, fee), plus
audited baseline/adjustment entries and a derived account-asset balance projection.
No fee support exists in current position math: preserve gross instrument P&L,
record fees in native currency here, and expose them through the deployment GET
response with an explicit gross-P&L label. Do not invent FX conversion or subtract
third-asset fees directly from quote P&L. Late fee enrichment inserts one fee entry
transactionally; a conflicting revision suspends for review rather than mutates
history. Fee-asset holdings reconcile against this ledger and local AssetLedger.
Expose audited baseline/adjustment POST
`/admin/strategies/{code}/deployments/{id}/adjustments` with reason, native amounts
and operator identity; only stopped deployments can change approved baselines.

- [ ] Write failing unit/integration tests for runtime-first, follow-first and concurrent arrivals; one position movement and fee charge; restart after inbox commit before booking; crash after synthetic order before fill; duplicate insert race; sequence gap rejection and replay ACK; venue time preserved; fees arriving later.
- [ ] Add `legacy_external_trade_is_not_rebooked`: legacy synthetic UUID is based on strategy/symbol/trade ID. Register approved historical account identity and map existing `venue_trade_id` records before enabling v2; ambiguous historical assignments block registration rather than invent a new UUID and double-book.
- [ ] Run `cargo test -p rustoms colo`; run ignored integration tests only with disposable DB.
- [ ] Implement durable inbox, retry worker and common economic identity mapping. Keep central order engine as booking authority; durable records must retain processing until booking acknowledged/verified. Do not delete from append-only `order_event`. Preserve original trade timestamp and use the native-asset ledger above for fees; register adjustment endpoint/schema in `src/main.rs` and refresh OpenAPI. Runtime lifecycle feeds monitoring, not path ① order routing.
- [ ] Feed independent venue follow through same dedup boundary. Use persistent per-symbol recovery cursors instead of 24-hour heuristic for v2 deployments. Keep legacy behavior isolated until migration. Verify migrations from 0040 against fixture of legacy fills without modifying their original accounting history.

## Task 10: Embedded runtime orchestration and replay reporter

**Files:** Create `crates/colo-runtime/src/{runner,reporter}.rs`, `tests/runner.rs`; expose public API in `lib.rs`.

**Interfaces:** `Runtime::start(config: RuntimeConfig, venue: Arc<dyn RuntimeVenue>) -> Result<Runtime, RuntimeError>`; `Runtime::submit(intent: Intent) -> Result<Submission, RuntimeError>`; `Runtime::cancel(client_id: &str) -> Result<(), RuntimeError>`; `Runtime::next_event() -> Option<RuntimeEvent>`; `Runtime::shutdown() -> Result<(), RuntimeError>`. Async methods remain in-process calls. `RuntimeConfig` carries deployment identity, two link URLs, local credential reference, journal path and explicit bounded queue/disk limits. No hardcoded unlimited/default allocation.

- [ ] Write failing fake-I/O tests: no send before Ready or journal durability; authority expires during journal flush -> no send; Kill racing prepared order prevents later send; blocking 10-second venue request cannot stop watchdog processing; journal failure stops admission; reporting backlog does not block kill; disk near limit stops new risk but attempts cancel; reconnect replays until durable ACK; one stalled strategy callback cannot hold execution/control processing.
- [ ] Run `cargo test -p colo-runtime --test runner`; expect missing runner/assertion failures.
- [ ] Wire serialized Book/reducer with independent control/watchdog, bounded journal worker, venue I/O tasks and reporter. Record reservation before asynchronous journal work; recheck policy/epoch/expiry before irreversible send. If policy changed, revalidate existing prepared intent without double-reserving. Account for fill/kill arriving before submit acknowledgement. Do not await network/disk under risk-state mutex or reducer turn.
- [ ] Reporter sends contiguous batches from journal, applies durable ACK and bounds replay memory. Stop new admission before capacity exhaustion; retain unacked obligations. If journal is unavailable, still attempt cancellation and flag inability to claim durable admission-stopped ACK. Runtime API exposes readiness and health; cancellation result means requested unless terminal proof exists.
- [ ] Run runtime, link and gateway focused suites together. Use memory/failure-injected journal for deterministic sequencing tests and real temporary-file journal for durability integration.

## Task 11: Migrate reference strategy and document actual readiness

**Files:** Modify `crates/colo-example/{Cargo.toml,src/main.rs,src/quote.rs}`, workspace manifests/lockfile; remove temporary `colo-risk` compatibility after all uses migrated. Update `docs-site/src/content/docs/strategies/colocated.md`, architecture page/component, design and HANDOFF.md if requested as ongoing handover.

**Interfaces:** Example consumes runtime submit/cancel/events and bootstrapped instrument rules. Local arguments include deployment ID, OMS address, credential reference and journal directory; symbol must belong to registered universe. No raw adapter send, manual OMS-position overwrite or `Vec` that forgets failed cancellations. Move deliberate bypass test behavior out of production example into isolated fake-venue tests.

- [ ] Write failing example unit tests for strategy decisions without venue access, paused-until-ready behavior and price/quantity generation obeying snapshot tick/step. Existing quote tests remain meaningful.
- [ ] Run `cargo test -p colo-example`; expect interface/behavior failures.
- [ ] Replace example execution plumbing with runtime. Startup validates local secret reference, registration and recovery; errors explain cause without logging credentials. Keep trading logic small. Remove `RiskBook` legacy state and obsolete crate references after confirming all consumers, including manifests and docs.
- [ ] Document registration, explicit timings/inventory approval, first start, restart, manual takeover, kill stages, event replay, journal storage needs and v1 upgrade. Show two asynchronous v2 connections on architecture illustration. Mark only completed components shipped; retain performance/live validation gates. State synchronous local journal cost and lack of independently enforced fencing.
- [ ] Run `cargo test -p colo-example`, `cargo check -p colo-example` and `npm run check` in `docs-site`. Confirm no FIX native dependency enters default colo example build.

## Task 12: Whole-system failure verification and latency evidence

**Files:** Create `crates/colo-runtime/tests/system.rs`, `crates/colo-runtime/examples/latency.rs`; add isolated-DB test runner under `scripts/test-colo-isolated.sh` using repository conventions, and verification results in `docs/superpowers/plans/2026-11-07-colocated-runtime-verification.md`.

**Interfaces:** Test runner must require an explicit disposable `DATABASE_URL`/equivalent connection selection, validate target provenance and `OMS_CONFIG=/nonexistent`, avoid loading dotenv, and never default to shared dev configuration. Adapt existing ignored test support to require explicit test DB selection for this runner. Do not execute unrelated existing ignored tests that contact live/testnet endpoints without fixture isolation.

- [ ] Write failure-driven end-to-end tests with local TLS control server, fake Binance responses/events, temp journal and isolated OMS database: startup/recovery, order/fill/cancel, policy cut, control loss, venue loss, slow I/O, duplicate reporters, OMS/runtime crash, inbox replay, legacy migration and attempted takeover. Assert accounting and local reservations converge without duplicate booking.
- [ ] Run `cargo test -p colo-runtime --test system` and disposable DB suite; prove each named failure is exercised rather than merely serializing expected messages. Check no default test targets real database or venue.
- [ ] Add reproducible benchmark emitting admission and market-data-to-send p50/p99/p99.9, sample count, errors, hardware, filesystem, journal mode, load and burst settings. Compare enabled durability with in-memory test baseline without offering unsafe production bypass. Include policy traffic and backlog; exclude remote exchange latency from local metric and label it clearly.
- [ ] Run `cargo test --workspace`, cockpit `npm test` only if cockpit contracts changed, docs `npm run check`, and `git diff --check`. Exercise required ignored tests only in isolated harness. Record exact commands/results and unresolved environment blockers; never call testnet/live verification complete without evidence.
- [ ] Review full branch/diff against spec. Update target/shipped labels based on evidence and retain unverified production/latency claims as open gates. No release, live order, migration of user DB, commit or push without explicit request.

## Plan self-review and handoff

Coverage: pure/shared semantics (1–2), complete venue reports (3), local atomic
state and assets (4), durability/replay (5), v2 transport (6), OMS policy and
ownership (7), recovery/authority (8), idempotent accounting (9), integrated
execution (10), example/docs (11), full verification/performance (12).

Deployment timing and opening inventory are required inputs, not guesses or
implicit defaults. Existing balances are imported as an explicit approved
baseline, not synthetic exchange trades; transfers/manual changes suspend until
attributed. Production latency acceptance remains a measured rollout gate.

Execution recommendation: subagent-driven task implementation and review, because
reservation/durability/accounting interfaces have correctness consequences that
benefit from independent review. Native inline execution is available if preferred.
Obtain plan review and execution-method choice before implementation.
