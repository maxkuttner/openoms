# Embedded co-located execution runtime

Date: 2026-11-07
Status: architecture approved by user on 2026-11-07; not implemented

## Intent and decisions

Keep latency-sensitive strategy execution in one process near the venue. The
strategy calls an embedded runtime through Rust APIs, not REST, IPC, or the OMS.
The OMS distributes configuration and policy, supervises execution, and maintains
durable consolidated accounting asynchronously. Normal order admission requires
neither a remote database lookup nor a synchronous OMS acknowledgement.

The user selected an embedded runtime and asked for a design and matching
architecture illustration, then approved the design. Dedicated accounts and
Binance Spot are the initial implementation scope. Deployment-specific authority,
freshness and inventory inputs remain explicit configuration. The written design
retains durable-before-send journaling: asynchronous journaling was subsequently
discussed as an alternative but was not selected or applied to this document.

### Approaches

| Approach | Benefit | Trade-off / decision |
|---|---|---|
| Deploy the existing OMS near the venue | Reuses current transactional execution | Still requires strategy-to-OMS communication and database admission; rejected for the latency-sensitive path |
| Separate local execution gateway | Can isolate credentials and enforce independently of strategy | Adds IPC and scheduling overhead; defer unless independent enforcement is required |
| Embedded runtime | Direct calls, local admission and immediate execution state | Trusted strategy, shared failure domain; selected architecture |

Crate boundaries do not imply process boundaries. This is an engineering choice,
not a claim about proprietary implementations at Jane Street or another firm.

## Current implementation and changes

Today path ① uses `src/domain/orders/risk.rs`, `src/risk_engine.rs`, and
`src/strategies/guard.rs` within the OMS admission transaction. Path ② links
`colo-risk`, `colo-link`, and `gateway` into `colo-example`. Its risk book uses
OMS-supplied positions and does not reserve outstanding order exposure. The OMS
independently follows the venue account and books external fills.

The target replaces the example's ad hoc execution management with a reusable
runtime. Rename/extract `colo-risk` into shared `risk` rules after specifying
behavioral differences; add a proposed `colo-runtime` crate to own execution.
Extend `colo-link` for bootstrap, policy, authority, recovery and event delivery.
Reuse `gateway` adapters; extend their contracts where recovery or event identity
is insufficient. These names describe intended responsibilities, not existing APIs.

The OMS path keeps its transactional admission and commit-before-send boundary.
It calls the same risk evaluator with a coherent position/working-order snapshot.
It does not acquire a second in-memory source of truth by embedding the colo
execution state alongside its database state.

## Target topology

```text
Central OMS: policy + budgets + supervision + accounting
       | snapshot, versioned updates, authority, kill
       |                                      ^ lifecycle / fills / health
       v                                      | replay + durable ACK
Co-located process                            |
  strategy -> colo-runtime -------------------+
                 | risk: evaluate + reserve locally
                 | local positions / order lifecycle / recovery journal
                 v
              gateway <----------------------> venue
                 ^ market data and execution reports

OMS venue-follow connection -> independent reconciliation / deduplicated booking
```

The runtime owns immediate execution exposure. The OMS owns consolidated durable
accounting and administrative policy. Exchange state must be reconciled after
gaps; neither an OMS snapshot nor a restarted empty local book proves no exposure.

## Scope and non-goals

First implementation: trusted Rust strategies, Binance Spot capability coverage,
one active runtime per dedicated venue account/subaccount, one strategy allocation.
The dedicated account is an enforced configuration invariant, not merely a tag.
Verify account/environment identity during setup; reject duplicate assignments
where identity can be established, and require explicit operator validation otherwise.

Do not initially support shared-account attribution, distributed portfolio-margin
netting, automatic active-active execution, arbitrary untrusted strategy plugins,
or automatic process deployment. Bootstrap configures an already provisioned
process; it is not a scheduler or remote code execution service. Other adapters
must satisfy the same capability/recovery contract before admission is enabled.

## Component responsibilities

| Component | Responsibility |
|---|---|
| `risk` | Validated units and inputs, policy resolution, directional exposure, state and price rules, stable decision reasons |
| `colo-runtime` | Atomic admission, reservations, positions, order reducer, rate windows, journal, recovery and watchdog |
| `colo-link` | Authenticated versioned control and event protocol, acknowledgements, replay and reconnect |
| `gateway` | Venue protocol, canonical event mapping, order status/open-order/fill queries and capability declarations |
| OMS | Policy/budget ownership, operator commands, deduplicated booking, monitoring and independent reconciliation |

Strategy sees submit/cancel operations, market data and execution notifications.
Raw adapter access is not part of its normal execution API. This discourages
accidental bypass but is not a security boundary inside a trusted process.

## Bootstrap and cached configuration

Bootstrap is authenticated using a deployment identity bound to strategy, account,
venue and environment. The OMS sends a complete immutable snapshot containing:

- Schema version, snapshot identity, policy revision and execution epoch.
- Canonical instruments, venue mappings, quantity/price increments, multipliers,
  supported order types and valuation currencies.
- Permissions, effective per-instrument overrides, default limits and allocated
  budget; explicit Active / Reducing / Halted state.
- Feed/session settings, freshness limits, authority duration and recovery policy.
- Accounting baseline and reconciliation watermarks, clearly marked as a baseline.

Provision venue credentials through restricted secret delivery or local secret
references, not general snapshot logs or event payloads. Cache non-secret metadata
in memory; persist the accepted configuration identity needed to interpret replay.

Policy is installed as a complete effective version at an admission boundary.
Reject incompatible schemas, revision rollback and wrong identity. ACK the exact
installed revision/epoch. On sequence gaps, suspend admission until a full snapshot
or validated replay restores continuity. Policy changes cannot interleave halfway
through one decision. Tightening a limit triggers reevaluation and cancellation
of affected working orders where needed; cancellations do not instantly remove risk.

## Local execution and risk

A single serialized admission/reducer owner per account processes decisions and
execution events. Network requests run independently and return results to that
owner; no network await holds up watchdog or state processing. Additional strategy
threads submit through this owner. No second process may spend the same allocation.

Before sending, validate inputs, policy freshness, authority, instrument/feed health,
state and limits; then atomically reserve exposure and record a unique intent.
Use canonical instrument/account identity, not symbol alone. Specify quantity,
notional currency and contract multiplier explicitly; do not silently treat all
quote currencies as USD. Reject non-finite values. Use configured fresh valuation
inputs for position risk, not an arbitrarily cheap candidate price.

Directional worst-case exposure includes pending and working orders:

```text
worst long  = position + remaining buys + candidate buy
worst short = position - remaining sells - candidate sell
```

Opposite orders do not offset unless the venue supplies an explicitly modeled
execution guarantee. Strict Reducing cannot cross flat; reducing orders reserve
the same finite reducible position. Rate counters have an explicit scope and count
policy, including retries; reuse consistent semantics rather than silently applying
the current colo rate limit to every existing OMS order.

For Spot, the runtime additionally reserves account-wide spendable assets across
instruments: buys consume quote currency including a conservative fee allowance;
sells consume base inventory. BTC/USDT and ETH/USDT therefore share the same USDT
capacity. Maintain one asset ledger with free/committed amounts, reconciling venue
free plus locked balances without subtracting local working reservations twice.
Market orders require a bounded spend/quantity and fresh conservative valuation;
reject orders whose maximum asset requirement cannot be bounded. External balance
changes suspend admission until attributed. Opening strategy inventory defines
the baseline for strict Reducing; accounting treatment remains a review decision.

### Order lifecycle

Track pending-send, live, partially filled, pending-cancel/replace, unknown outcome,
and terminal outcomes. A fill moves quantity from working exposure to position in
one reducer step. Submission timeout remains charged; do not blindly retry with a
new identity. Cancel capacity releases only on authoritative terminal evidence.
Replace reserves worst-case overlap unless venue guarantees atomic replacement.
Late fills still apply even after local cancellation assumptions; trade identities
prevent duplicate application. Terminal notification alone must not lose fill details.

## Journal, OMS events and accounting

Proposed initial durability contract: append intent/reservation to a local journal
and make it durable before sending. Persist state transitions and outbound events
with monotonically increasing sequence numbers. This adds local storage latency,
not an OMS/database network round trip. Measure its tail latency before claiming a
latency target; weaker durability requires a separate explicit design decision.

Delivery to OMS is at least once with replay after reconnect. ACK only after OMS
durably accepts the event. Persist ACK/replay state and retain unacknowledged data.
Bound queues and disk usage; halt new risk before exhaustion rather than drop
execution events. Keep cancellation and reconciliation available where possible.

Send order intents/submission outcomes, venue lifecycle reports including fills,
fees and original timestamps, policy/command acknowledgements, health, and
checkpointed position/open-order summaries. A fill is one economic event even if
reported by both runtime and OMS venue-follow stream. Deduplicate on stable venue,
account, environment and execution identity; validate adapter-specific uniqueness.
The runtime sequence orders transport events, not independent venue observations.

Both sources feed the existing OMS booking authority. Missing lifecycle information
may be enriched later without booking the same fill twice. OMS position checkpoints
must carry a known execution watermark; never overwrite newer local exposure with
an unversioned snapshot. Disagreement stops new risk until resolved, not reset to flat.

## Startup, recovery and execution authority

States: Disconnected -> Bootstrapping -> Recovering -> Ready; any integrity failure
returns to Suspended. Active policy alone does not imply Ready.

Suspended, expired-authority and killed states permit cancellation and recovery
queries only, not new orders (including reducing orders). Reducing submissions
require Ready plus valid authority and an explicit Reducing policy. Persist a
kill latch before acknowledging admission stopped. Ordinary Active snapshots,
heartbeats and restarts cannot clear it. Resume requires a newer explicit OMS
Enable command referencing the stopped epoch/command, current policy, renewed
authority and successful recovery. Integrity suspensions additionally require
their cause resolved; unexplained accounting differences require operator approval.

Before Ready:

1. Authenticate deployment; acquire a single-owner epoch and install policy.
2. Replay journal, retaining pending/unknown commitments.
3. Establish venue event stream and query open orders, recent executions and the
   configured position/balance baseline with an overlap-safe recovery boundary.
4. Deduplicate replay/query/stream events, resolve unknown sends and reconcile
   positions and outstanding orders. Unavailable history or unexplained balances
   require operator resolution; an empty query after error is not a flat account.
5. Confirm health, current authority and consistent state before admitting orders.

Persistent cursors and journal identities survive restart. Client order IDs include
deployment identity and persistent sequence; they must not restart at zero. Recovery
must not depend on discovering symbols only from currently open orders.

Authority is bounded and renewed by the OMS for an exact epoch and policy. Use
local monotonic receipt-time deadlines, not queued-message consumption time. On
expiry, stop new risk and initiate cancellation. Existing positions/orders remain
charged. The watchdog must run independently of slow venue calls and strategy logic;
a deadlocked/crashed process can still defeat an in-process watchdog.

No automatic replacement until the previous owner is known stopped and venue state
is reconciled. A database lease or epoch cannot fence a process that retains valid
venue credentials and bypasses the runtime. Where hard fencing is needed, revoke
old credentials or use venue session controls before takeover. Otherwise require
manual verified shutdown; do not advertise independently enforced failover.

## Kill and venue protections

Distinguish command received, admission stopped, cancellations sent, and confirmed
clear. Attach command identity and epoch to acknowledgements. Kill stops new risk,
attempts cancellation locally and through the OMS's independent account access,
and keeps monitoring late fills/unknown outcomes. It does not flatten positions.

Cancellation is not guaranteed within five seconds and cannot prevent fills already
in flight. Declare venue-specific cancel-on-disconnect/countdown capabilities;
Binance futures mechanisms must not be assumed for Spot REST. A process crash needs
external monitoring or venue protection; shared risk code alone cannot provide it.

## Migration and verification

1. Specify shared semantics and extract `risk`; preserve current OMS admission
   locking and commit-before-send while correcting coherent exposure reads,
   directional working exposure and strict Reducing. Record intended changes.
2. Extend bootstrap/control protocol with effective policy, identity, epochs,
   versions and recovery watermarks; reject incompatible peers explicitly.
3. Build local reducer, journal, reservations and venue recovery; migrate
   `colo-example` to runtime APIs instead of raw execution calls.
4. Add replayable runtime events to OMS ingestion with cross-source deduplication.
   Keep independent venue follow active throughout migration.
5. Validate failure scenarios and benchmark before marking target as shipped.

Required scenarios: concurrent admissions; opposing working orders; partial fills;
late fills after cancel; unknown submission; replace overlap; Reducing capacity;
stale prices/policy; snapshot rollback; sequence gaps; duplicate feeds; restart with
live orders; unavailable history; disk/queue exhaustion; expired authority; duplicate
process; kill during slow venue I/O; OMS loss while venue remains reachable.

Measure market-data receipt to order send plus admission p50/p99/p99.9 under load,
including durable journal writes, policy updates, bursts and recovery. No latency
guarantee is asserted by this design. Testnet verifies adapter behavior; it does not
prove every live venue failure mode. Shared database ignored tests remain restricted
to isolated disposable infrastructure.

## Deployment decisions and performance gate

- Use dedicated-account / single-active-runtime first release and Binance Spot scope.
- Set authority expiry and market-data freshness values per deployment; no implicit
  continuation indefinitely after OMS loss.
- Confirm local durable-before-send journal contract against measured latency budget.
- Confirm accounting baseline for existing balances, deposits/transfers and manual
  activity; unexplained changes suspend admission until reconciled.

The architecture documentation illustrates this as a proposed target separately
from the shipped diagram. No implementation or rollout is implied by publishing it.

Implementation plan: `docs/superpowers/plans/2026-11-07-colocated-runtime.md`.
