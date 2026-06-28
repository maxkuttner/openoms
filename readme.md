# OpenOMS

**TODO:**

**Instrument Seeding**
- [ ] cache Databento `definition` fetches to `.dbn` so resets replay offline (no refetch)
- [ ] `EQUS_SUMMARY` is consolidated like the removed `DBEQ.BASIC` — same symbol-spans-venues collision risk if enabled

## Roadmap — REST OMS now → low-latency execution engine

Vision: ship a correct REST OMS (**system of record + governance + oversight**), then evolve the
same Rust, event-sourced core into a **low-latency, execution-capable** system — to serve
systematic / quant funds (QRT-scale and smaller), not just discretionary buy-side. Latency
ladder: **ms (REST) → sub-ms (in-memory + FIX) → μs (lock-free / colo)**. Feature benchmark in
`docs/oems-feature-gap.md`.

The reusable asset across every phase is the **domain core** (event-sourced order aggregate,
lifecycle, risk, positions). Phases rebuild the I/O + persistence layers, not the brain.

### Phase 1 — OMS core (REST · system of record + governance)

Done: event-sourced order SoR + audit log · entitlements (principal × portfolio grants) ·
pre-trade risk + trading-state HALT · positions + P&L · multi-broker routing · post-trade
allocation. (A client needs only a **principal + portfolio** to trade; account is custodial-only
and inferred from the portfolio's default route. No-creds fixture seeds `alpaca-paper` + a test
user `test-trader-key` : `test-secret`.)
- [ ] **amend/replace + cancel fully wired** API→broker→event (`ReplaceOrder`/`CancelOrder` exist)
- [ ] **blotter / oversight query API** — orders, fills, positions across principals/portfolios,
      filterable ("who is trading what"); today only `GET /orders/:id`
- [ ] **broker/custodian reconciliation** — match our records against broker positions/fills

### Phase 2 — oversight & control depth (REST)

- [ ] **central kill-switch / trading-halt** — HALT a portfolio / instrument / principal on demand
- [ ] **drop-copy / external-execution ingestion** — report orders + fills executed *elsewhere*
      into the OMS, so it has central oversight even off the execution path (the quant bridge)
- [ ] **light mandate compliance** — restricted/blocked lists; concentration / leverage (w/ marks)
- [ ] **finer entitlements & risk** — per-instrument / per-strategy limits
- [ ] **market data / P&L marks** — unrealized P&L, exposure valuation
- [ ] optional **maker-checker approval** — configurable, not a mandatory gate

### Phase 3 — execution capability + latency foundation (the pivot)

- [ ] **decouple the hot path from Postgres** — in-memory authoritative order/risk/position state
      + **async event journal**; Postgres becomes a downstream projection (event-sourcing done
      right; ms → sub-ms; prerequisite for everything below)
- [ ] **FIX / binary order entry** (persistent sessions) alongside REST
- [ ] **direct venue connectivity** (exchange gateways) + **L2 market data** (order books)
- [ ] **SOR + execution algos** (TWAP / VWAP / POV) + order slicing; per-connection execution
      streams; crossing (internal netting)

### Phase 4 — low-latency hardening (mid → high frequency)

- [ ] thread-per-core / lock-free / no-allocation hot path, pinned threads, busy-poll
- [ ] binary wire protocol (e.g. SBE), kernel-bypass networking
- [ ] colocation; in-line μs pre-trade risk

### Discretionary add-ons (as needed)

- [ ] pre-trade **baskets** + bulking; across-accounts allocation grain; best-execution logging

### Out of scope

Settlement-instruction generation + venue-level regulatory reporting (broker/custodian's job);
full portfolio analytics (rebalancing, index/model tracking, NAV / what-if, OTC RFQ).
