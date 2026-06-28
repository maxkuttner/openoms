# OMS Feature Gap Analysis — vs a professional OEMS

> **Note:** the *prioritization* below was written for a small-buy-side target. The live,
> broadened roadmap (small + mid/higher-frequency funds; REST OMS → low-latency execution
> engine) lives in the README "Roadmap" section. This doc stays as the feature *catalog* /
> benchmark; the README is the authority on ordering.

What our OMS needs to be a *viable professional OMS*, benchmarked against two real
institutional references:
- **FIS Front Arena (PRIME) Buy-Side OMS** User Guide (`FCA4750-OMS.pdf`) — the buy-side
  PM → compliance → trader workflow.
- **ZKB / Front Arena "Execution & Order Management"** deck (`order_management_oe.pdf`) — the
  bank/sell-side execution view: FIX order flow, STP routing rules, the Sales/Execution desk
  split, and regulatory reporting.

## Target segment

**Small buy-side: hedge funds, family offices, small trading firms.** This is decisive — it
moves much of the "professional OMS" weight *out* of scope, because for a small fund the
**prime broker / custodian** carries the institutional plumbing:

- The fund **is** the order source (PMs / traders / strategies via our API/UI), so **inbound
  FIX order entry is not needed**. Outbound connectivity can stay broker REST adapters; FIX-out
  is only for broker breadth later.
- The **executing broker reports to the venue** and the **custodian settles**, so venue-level
  **regulatory trade/transaction reporting and settlement-instruction generation are largely
  the broker's job**, not ours — we need clean records + **reconciliation against the broker**,
  not a reporting/settlement engine.
- Compliance is **investment-mandate** scale (restricted lists, concentration, leverage),
  **not** a bank's auditable regulatory rule engine. Teams are small, so **maker-checker
  approval is optional/configurable**, not a mandatory multi-desk gate.

What this segment *does* pull **into** scope that a pure routing engine wouldn't have:
**position-keeping + P&L** and **allocation across funds/accounts** — a fund must know its book
and split blocks across its accounts. Full portfolio analytics (rebalancing, index/model
tracking, NAV/what-if, OTC RFQ) stay **out**.

The tiers below catalog the full professional picture; the **"Prioritized for our segment"**
section re-ranks them for small buy-side.

## What a professional OEMS does (Front Arena, condensed)

- **Role-separated workflow / segregation of duties:** PM (generate trades) → Risk &
  Compliance (approve / reject) → Trader (execute). Three distinct views, three user groups.
- **Compliance engine (headline differentiator):** an optional client-side *local* check plus
  a **mandatory server-side *pre-deal* check** that gates the workflow and writes an
  **immutable, timestamped compliance report** for regulators. Rule sheets with breach /
  warning states; Approve-breaches / Retry-check / Reject.
- **Trade program** = the central object: a bundle of orders/trades. Compliance and workflow
  act on the *program* (all-or-nothing release), not on individual orders. Backed by a
  business-process state machine with full history.
- **Execution:** order states (Inactive → Accepted), **bulk orders** (aggregate same
  instrument/side to cut cost), **manual fill** (non-electronic), **reallocate** bulk fills
  across funds, cancel, **Move-to-Market** routing to per-venue adapters, splitting an order
  across **multiple destinations / batches**, a fills view, OTC pricing.
- **Allocation:** trades carry their place in the portfolio structure; bulk → reallocate.
- **Cross-asset:** equities, FX, NDF, futures, OTC (handled as "reserved trades", executed by
  confirmation with no exchange order), combinations / indices.
- **Integration ergonomics:** import/export order programs (CSV/Excel), custom rebalancing
  actions via a scripting extension.
- **Portfolio trade generation (Tier 3 for us):** candidate trades + what-if, column-based
  trading, index/model/stored-weight tracking, future roll, NAV/weights.

## What we already have

Event-sourced order core (`order_event` append-only ledger + `order_state` projection),
lifecycle state machine (`src/domain/orders`), pre-trade **risk** (qty/notional limits +
trading-state gate, keyed portfolio×instrument), entitlements (principal×portfolio grant),
identity (principal / portfolio / account / broker_connection), multi-broker routing
(`BrokerRegistry`, `broker_instrument`, portfolio-default route + override), instrument
master, Alpaca adapter + execution stream (fills), admin REST API, api-key auth, Kafka event
publishing. Order types Market/Limit; TIF Day/GTC/IOC/FOK.

## Gap analysis — necessary features, prioritized

### Tier 1 — institutional must-haves to be "viable"

1. **Compliance rule engine + auditable pre-trade gate.** We have *quantitative risk*, not
   *rule-based compliance* — restricted/blocked lists, issuer & sector concentration, exposure
   mandates, regulatory limits — producing an **immutable, timestamped report**. The #1
   institutional differentiator, and we have none of it.
2. **Approval workflow / segregation of duties** (maker-checker): submit → *pending approval*
   → approve / reject → route. Today orders route straight through. Needs an approval gate, the
   states, and an "approve" entitlement distinct from "trade".
3. **Order grouping (parent / program) + all-or-nothing release.** We have flat orders; bundle
   related orders so compliance and approval act on the group. (Aligns with the README "Order
   Groups" note.)
4. **Amend/replace + cancel, fully wired end-to-end.** `ReplaceOrder` / `CancelOrder` exist in
   the domain; confirm both are wired API → broker → event with ack/reject handling.
5. **Blotter / query API.** Orders & fills by state / portfolio / instrument / time /
   connection — an operational necessity; today only `GET /orders/:id`.
6. **FIX protocol connectivity** (inbound *and* outbound). Institutional clients send orders
   and venues/brokers accept them over **FIX** — `NewOrderSingle`, `ExecutionReport`,
   `OrderCancel/Replace Request`, `AllocationInstruction`, `Quote`. We are REST-only with
   per-broker adapters (Alpaca REST, IBKR stub). Without a FIX gateway (in: order entry;
   out: venue routing) institutional order flow simply can't reach us. *Caveat:* how Tier-1
   this is depends on target segment — foundational for bank/institutional clients, optional
   for an API-first niche. It's the single biggest *interoperability* gap.
7. **Rule-based pre-trade gating / STP vs. manual handling.** Auto-route ("DMA") when safe,
   else **stop the order for a manual desk** ("Care"), driven by rules: handling instruction,
   order-value threshold, limit-vs-reference-price deviation (e.g. >5%), instrument-type /
   client eligibility ("client not set up for bonds → reject"). A *routing/gate* rule engine,
   distinct from compliance (item 1) and quantitative risk. Today routing is unconditional.
8. **Regulatory trade & transaction reporting** (market transparency / MiFID II): an automatic
   near-real-time **trade report** (e.g. ≤1 min for SIX-listed equities) plus a **transaction
   report** (originator + txn id, ~T+1). A hard requirement to operate in regulated venues; we
   emit nothing today.
9. **Settlement / back-office handoff.** The lifecycle doesn't end at the fill — trades must
   **book to settlement** (Order → Trade → settlement instruction → custody/back-office, e.g.
   Avaloq). We stop at `order_state`/fills with no trade-booking or settlement output.

### Tier 2 — professional depth (several already on the README)

10. **Allocation, bulking & crossing.** Aggregate same-instrument/same-side orders (**bulking**),
    net opposite sides within one client (**crossing**), and allocate fills to accounts — both
    **pre-trade** (basket pre-assigned per fund) and **post-trade** (**shaping** from a broker
    warehouse account). (README "post-trade allocation".)
11. **Positions model** — a real `position` table maintained from fills (the `risk_engine.rs`
    TODO), replacing the order-derived exposure approximation; also unlocks P&L.
12. **Multi-venue routing / order splitting (slicing) / SOR** — split one order into child
    orders worked on different venues/strategies. (README "routing decision layer".)
13. **Manual fill entry** — non-electronic execution / reconciliation.
14. **Per-connection execution streams + more adapters** (README "generalize execution
    streams"; IBKR inbound).
15. **Market-data / pricing** — marks for notional + position valuation; reference price for
    the limit-vs-reference gate (item 7).
16. **Execution algos** (TWAP / VWAP / POV) — work a large order over time toward a benchmark.
17. **Best execution** policy + measurement (MiFID II) — pairs with reporting (item 8); evidence
    that orders were routed/executed on best available terms.

### Tier 3 — out of scope (buy-side portfolio system, not our lane)

Portfolio trading / rebalancing, index/model/stored-weight tracking, NAV / weights / what-if,
OTC RFQ pricing. Noted for completeness; not planned.

## Prioritized for our segment (small buy-side)

Re-ranking the catalog above for hedge funds / family offices / small trading firms. The big
shift: the heavy institutional stack (FIX, venue reporting, settlement) is **mostly the prime
broker's job**, while **position-keeping and allocation** move up.

**Build now — viable small-buy-side OMS:**
1. **Positions + P&L** (item 11) — a fund must know its book; build the `position` table from
   fills. *Elevated from Tier 2.*
2. **Allocation across funds/accounts** (item 10) — pre- and post-trade (shaping); core for
   multi-fund / family-office / SMA setups. *Elevated.*
3. **Order groups / baskets** (item 3) — rebalance/basket submission; substrate for compliance
   + allocation.
4. **Light pre-trade mandate compliance** (item 1, *scoped down*) — restricted/blocked lists,
   concentration, leverage limits, layered on the existing risk engine. **Not** an auditable
   regulatory rule engine.
5. **Amend/replace + cancel fully wired** (item 4) and **blotter / query API + basic UI**
   (item 5) — operational table stakes.
6. **Optional/configurable approval** (item 2) — maker-checker as a toggle (family-office
   sign-off), not a mandatory multi-desk gate.

**Later — depth:**
Market data / P&L marks (15), **broker/custodian reconciliation** (reframed 9), more broker
adapters / FIX-*out* (14, 6-out), SOR / splitting / algos (12, 16), crossing (10), manual fills
(13), light best-execution logging (17).

**Out of scope for this segment:**
Inbound FIX order entry (6-in), venue-level regulatory trade/transaction reporting (8),
settlement-instruction generation (9), full portfolio analytics (Tier 3).

**Lead recommendation:** the highest-value next build is **positions + allocation** plus a
**light mandate/limit layer on the existing risk engine** — *not* the institutional
FIX/reporting/settlement stack. That's deliberately the opposite of what the bank deck implies,
and it's the right call for small buy-side.

---
*Sources: FIS Front Arena (PRIME) Buy-Side OMS User Guide (`FCA4750-OMS.pdf`); ZKB / Front
Arena "Execution & Order Management" deck (`order_management_oe.pdf`). Both in
`~/Documents/oepfelbaum/knowledge/`, which also has `Overview Front Arena.pdf` / `Overview
PRIME.pdf` for wider coverage later.*
