# OpenOMS

**TODO:**

**Instrument Seeding**
- [ ] cache Databento `definition` fetches to `.dbn` so resets replay offline (no refetch)
- [ ] `EQUS_SUMMARY` is consolidated like the removed `DBEQ.BASIC` — same symbol-spans-venues collision risk if enabled

**Multi-broker routing — done.** Identity rework complete: a client needs only a
**principal + portfolio** to send orders; account is custodial-only and inferred from the
portfolio's default route (optional override); route recorded on
`order_state.broker_connection_code`; grant is (principal × portfolio), risk is
(portfolio × instrument). The no-creds fixture seeds the `alpaca-paper` connection plus a
ready-to-trade test user (HTTP Basic `test-trader-key` : `test-secret`). Remaining
multi-broker tasks are folded into the roadmap below.

**OMS feature roadmap (small buy-side)**
From `docs/oems-feature-gap.md` (benchmarked vs Front Arena OEMS). Target = hedge funds /
family offices / small trading firms, so heavy institutional plumbing (FIX, venue reporting,
settlement) is the prime broker's job and stays out; position-keeping + allocation move up.

_Tier 1 — to be a viable small-buy-side OMS:_
- [x] **positions + P&L** — `position` table (portfolio × instrument) built from fills,
      average-cost + realized P&L; risk reads it (O(1)); `GET /portfolios/:id/positions`.
      Unrealized P&L pending market-data marks
- [ ] **allocation across funds/accounts** — pre-trade (basket pre-assigned) and post-trade
      (shaping), plus bulking of blocks
- [ ] **order groups / baskets** — group orders into a program for joint submit / compliance /
      allocation
- [ ] **light pre-trade mandate compliance** — restricted/blocked lists, concentration,
      leverage, layered on the risk engine (not an auditable regulatory engine)
- [ ] **amend/replace + cancel fully wired** API→broker→event (`ReplaceOrder`/`CancelOrder`
      exist in the domain)
- [ ] **blotter / query API** (+ basic UI) — orders & fills by state / portfolio / instrument /
      time / connection (today only `GET /orders/:id`)
- [ ] **optional maker-checker approval** — configurable toggle, not a mandatory gate

_Tier 2 — depth, after Tier 1:_
- [ ] market data / pricing — marks for P&L + notional risk
- [ ] broker/custodian reconciliation — match our records against broker positions/fills
- [ ] generalize execution streams: one per broker connection (currently Alpaca-only in
      `alpaca_stream.rs`); add IBKR inbound (adapter exists, no stream)
- [ ] more broker adapters + FIX-out (broker breadth)
- [ ] routing decision layer — explicit broker on order / SOR / order splitting (slicing)
- [ ] crossing — internal netting of opposite sides within a client
- [ ] manual fill entry (non-electronic / reconciliation)
- [ ] execution algos (TWAP / VWAP / POV)
- [ ] light best-execution logging
- [ ] separate account/connection routing permission (explicit-account override not
      entitlement-checked today)

_Out of scope (small buy-side):_ inbound FIX order entry, venue-level regulatory trade/
transaction reporting, settlement-instruction generation, full portfolio analytics
(rebalancing, index/model tracking, NAV / what-if, OTC RFQ).
