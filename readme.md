# OpenOMS

**TODO:**

**Instrument Seeding**
- [ ] cache Databento `definition` fetches to `.dbn` so resets replay offline (no refetch)
- [ ] `EQUS_SUMMARY` is consolidated like the removed `DBEQ.BASIC` — same symbol-spans-venues collision risk if enabled

**Multi-broker routing**
The identity rework is done: a client needs only a **principal + portfolio** to send
orders. Account is custodial-only and inferred from the portfolio's default route (with an
optional explicit override); the route is recorded on `order_state.broker_connection_code`;
the grant is (principal × portfolio) and risk is (portfolio × instrument). The no-creds
fixture seeds the `alpaca-paper` connection plus a ready-to-trade test user
(HTTP Basic `test-trader-key` : `test-secret`). Remaining multi-broker work:
- [ ] routing decision layer: explicit broker on the order / smart (SOR, algo-wheel) —
      today the broker is whatever the resolved account's `broker_connection` says
- [ ] generalize execution streams: one per broker connection (currently Alpaca-only in
      `alpaca_stream.rs`); add IBKR inbound (adapter exists, no stream)
- [ ] separate account/connection routing permission — an explicit-account override is
      not entitlement-checked today
- [ ] (advanced, only if multi-account) post-trade allocation across accounts —
      `AllocationInstruction`-style endpoint
