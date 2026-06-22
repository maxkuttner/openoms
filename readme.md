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

**4) Identity model & multi-broker routing**
Rework so a client only needs a trader + portfolio to send orders, and so routing
matches broker-neutral pro OMSs (Charles River, FlexTrade, Bloomberg). Today the
account fuses *custodial account* with *broker routing target* and is forced onto
every order. Architecture is already half multi-broker (`BrokerRegistry` keyed by
`(broker_code, environment)`, per-broker `broker_instrument`, `RouteOrder`/`OrderRouted`).
- [ ] rename `book` → `portfolio` to align naming with pro OMS (schema + API + code)
- [ ] split broker route/connection from account: account = custodial/allocation only;
      add a `broker_connection` entity (broker_code + environment + creds); orders route
      to a *connection*, not an account
- [ ] routing decision layer: explicit broker on the order / default bound to the
      portfolio / smart (SOR, algo-wheel) later — today routing is implicit via
      `account.broker_code` in `handlers.rs`
- [ ] drop `account` from the common `/orders/submit` path — default the route from the
      portfolio, allow an explicit override only
- [ ] generalize execution streams: one per broker connection (currently Alpaca-only in
      `alpaca_stream.rs`); add IBKR inbound (adapter exists, no stream)
- [ ] store the resolved broker/route on `order_state` (currently keys on `account_id`)
- [ ] re-key entitlements & risk: trade grant → (principal × portfolio); separate routing
      permission ("can this desk use IBKR?"); risk → (portfolio, instrument)
- [ ] (advanced, only if multi-account) post-trade allocation across accounts —
      `AllocationInstruction`-style endpoint
- [ ] minimal version for multi-broker without institutional weight: `broker_connection`
      + portfolio-default route (+ optional override) + route stored on the order
