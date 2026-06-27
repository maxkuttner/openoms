# OpenOMS

**TODO:**

**1) Order Groups**
I have decided that the client already gets the order objects 
for each leg of a composite trade. Therefore, it should be possible
to leave the trade leg management up to the client.
We could use the causation id (which meant for like trading strategies etc.) 
to identify correlated orders. However, I think by using a group id or thinking
about how orders relate to each other would also build the pressure to 
add endpoints to handle group ids, like cancellation, lifecycle management.

**2) Instrument Mappings**
- add broker/exchange <-> master instrument mapping 
    * this also means that there is some sort of interface such that users 
      can add new instrument mappings
- rethink whether the authentication should be based on principal or on account
- should we add an omnibus account? <= well we kinda have that already since we are using one 
  base account for the external broker connection/api

**3) Instrument Seeding**
- [ ] cache Databento `definition` fetches to `.dbn` so resets replay offline (no refetch)
- [ ] `EQUS_SUMMARY` is consolidated like the removed `DBEQ.BASIC` — same symbol-spans-venues collision risk if enabled

**4) Multi-broker routing**
The identity rework is done: a client needs only a **principal + portfolio** to send
orders. Account is custodial-only and inferred from the portfolio's default route (with an
optional explicit override); the route is recorded on `order_state.broker_connection_code`;
the grant is (principal × portfolio) and risk is (portfolio × instrument). The no-creds
fixture seeds the `alpaca-paper` connection plus a ready-to-trade test user
(HTTP Basic `ak_test` : `test-secret`). Remaining multi-broker work:
- [ ] routing decision layer: explicit broker on the order / smart (SOR, algo-wheel) —
      today the broker is whatever the resolved account's `broker_connection` says
- [ ] generalize execution streams: one per broker connection (currently Alpaca-only in
      `alpaca_stream.rs`); add IBKR inbound (adapter exists, no stream)
- [ ] separate account/connection routing permission — an explicit-account override is
      not entitlement-checked today
- [ ] (advanced, only if multi-account) post-trade allocation across accounts —
      `AllocationInstruction`-style endpoint
