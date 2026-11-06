# Strategies / algos path ①: FIX acceptor and strategy registry

Date: 2026-11-05. Status: design, agreed in conversation; not started.
Parent: `docs/superpowers/specs/2026-11-04-target-architecture-design.md` (branch `docs/target-architecture`), §4.4 path ① and §7 phase 7. That document asks for this design before phase 7 is built.

## 1. Intent

Quant developers and traders write their own strategies / algos in Python, C++ or Rust. Typical examples are crypto cross-exchange arbitrage and stat arb. They run on the developer's own servers, on the internal network, or on co-located servers reached over the internet.

Phase 7 lets such a strategy trade **through the OMS**. Every order it sends:
- passes the same pre-trade checks as a cockpit order;
- is booked by the same order engine;
- shows up in the same blotter and positions.

The OMS can stop the strategy at any time, without the strategy cooperating. The price is one extra hop, about 1–5 ms. Strategies that cannot afford that hop are phase 8 (path ②) and out of scope here.

A strategy reaches the OMS over **FIX**, which is new in this phase, or over the **REST API that exists today**.

## 2. Goals, non-goals, anti-goals

**Goals**
1. A test client logs on over FIX, places an order, and receives its execution reports. This is the parent document's "done when" for phase 7.
2. A strategy is set up entirely in the cockpit: identity, logon, connections, limits.
3. Kill stops a strategy within one round trip to each venue, whatever state the strategy is in.
4. A strategy can trade two venues from one session, for the two legs of an arbitrage.
5. REST for strategies is documented, and reuses today's trading tokens.

**Non-goals**
- Path ②: co-located strategies trading directly with venues, the Kafka activity topics, and the risk crate.
- Modifying a live order (FIX 35=G). Strategies cancel and send a new order instead.
- Client certificates (mutual TLS). A password inside TLS is enough for now.
- Market data to strategies. Strategies bring their own prices.
- Options or other instruments the OMS does not know.

**Anti-goals: these would be failures even if they technically work**
- A strategy order skips any check a cockpit order goes through, or is booked anywhere other than the order engine.
- Kill depends on the strategy behaving.
- A strategy's working orders survive after its session is gone.
- A rejected order reaches the strategy without a reason it can read.
- A FIX password is stored so that it can be read back.

## 3. Constraints

- **Repo rules:**
  - Rust workspace, one process.
  - Postgres is the system of record; migrations go under `db/migrations/ods/oms/`.
  - Single user, pre-release: no compatibility shims are needed.
  - No help text in the UI. Explanations go in the docs site, including the FIX spec for strategy developers.
  - DB tests are `#[ignore]` against the shared dev DB. Never create databases on the shared dev Postgres.
- **The order path:** strategy orders enter the **same order path** as cockpit orders. Today that is the body of `handlers::orders_submit` (`src/handlers.rs:328`). It has to be callable from the FIX acceptor without HTTP, so the builder extracts its core rather than copying it.
- **FIX engine:** the acceptor uses the `quickfix` crate already in `crates/gateway` (built with SSL). Execution reports go out to strategies from the order lane of the event bus, after the event-store commit (parent §4.1).

## 4. What was decided

### 4.1 Strategy registry (new config table, plus a cockpit page)

One row per strategy. It holds:
- **Identity:**
  - a name;
  - its **portfolio**, owned **exclusively**: only the strategy trades it. The cockpit can view it and use Kill, Enable and Flatten, but cannot place manual orders on it. Everything strategy-scoped (limits, state, Kill, cancels, Flatten, reports) is therefore keyed by portfolio, and orders need no separate strategy tag;
  - its owning user;
  - a **service principal** for REST, reusing the trading-token flow.
- **FIX logon:**
  - a SenderCompID/TargetCompID pair, unique across strategies;
  - a password, stored only as a hash (argon2 or similar). It is shown once when set or reset;
  - an optional **IP allowlist** (CIDRs); empty means any address.
- **Connections:** the connections it may use, **at most one per broker code** (`BINANCE`, `ALPACA`, `IBKR`, …). This is what resolves the routing code named in an order (4.2) to exactly one connection. A paper and a live connection of the same broker therefore never both belong to one strategy.
- **State**, stored on the strategy row: Active, Reducing or Halted. These carry today's `TradingState` meanings: Reducing admits only orders that shrink a position; Halted admits nothing.
- **Limits:** see 4.3.

### 4.2 What a strategy may trade

- **Rule:** any instrument the OMS knows that has a `broker_instrument` mapping on one of the strategy's connections.
- **How orders name instruments:** by **symbol plus where the order goes**: tag 55 Symbol plus tag 100 ExDestination in FIX, the same two fields in REST. Tag 100 is the routing code, the connection's broker code (`BINANCE`, `OKX`, `ALPACA`). Tag 55 is that broker's symbol, as in `broker_instrument`. For a crypto venue this is exactly the venue and its own symbol (`BTCUSDT` on `BINANCE`). For a broker that reaches many exchanges (Alpaca, IBKR), the order names the broker and the broker's symbol. The OMS translates through `broker_instrument`, and rejects an unknown pair with its reason.
- **Discovery:** the strategy asks with a FIX Security List Request (35=x) and gets back a Security List (35=y) of its tradeable instruments: venue, symbol, OMS instrument id. A REST endpoint returns the same list as JSON.

### 4.3 Pre-trade checks

Every check a cockpit order gets, plus:
- **Per-strategy limits:** max order quantity, max order notional, max position quantity, max position notional.
  - **Per instrument.** Each limit caps one instrument, and a strategy-wide default fills in for instruments with no override. There is no total across instruments yet: that needs an FX source.
  - **Currency.** Notional is in the instrument's quote currency; the OMS has no FX conversion. So the strategy-wide notional defaults apply only to instruments quoted in **USD, USDT or USDC**. An order on any other quote currency (`ETHBTC`) needs a per-instrument notional override, or it is rejected (`no notional limit for quote currency BTC`).
  - These extend today's `risk_limits` (portfolio, instrument), which has no portfolio-wide default yet.
- **Price band (fat-finger check):** a limit price more than X% away from the OMS's live mark is rejected. X is set per strategy.
- **Market orders:** a strategy's market order is measured against the live mark for its notional checks. Today market orders skip them; cockpit orders keep today's behaviour.
- **No mark yet:** the OMS subscribes the instrument through the market data gateway (`MdHandle::subscribe`, phase 6) and **holds the order** until the first quote, then checks it. It stays subscribed while the strategy is logged on.
- **Rejections** go back as ExecutionReport 39=8 with a readable reason in tag 58, for example `max_order_notional: 12000 > 10000`.

### 4.4 FIX session

- **Transport:** TLS on every session, internal network included.
- **Logon:**
  - unknown CompIDs: the connection is dropped;
  - wrong password (tag 554), an address outside the allowlist, a Halted strategy, or a second session while one is live: Logout (35=5) with the reason in tag 58.
- **In** (strategy → OMS):
  - New Order Single (35=D): 11 ClOrdID, unique per strategy; 55 + 100; 54 side; 38 qty; 40 type (market, limit); 44 price; 59 TIF (day, GTC, IOC).
  - Order Cancel Request (35=F), naming the order by ClOrdID.
  - Order Status Request (35=H).
  - Security List Request (35=x).
- **Out** (OMS → strategy):
  - ExecutionReport (35=8) for every order event the order engine records on **orders the strategy sent over FIX**: new, partial fill, fill, cancelled, rejected. Also for each 35=H, and for pre-trade rejections. Flatten and REST orders produce none.
  - Order Cancel Reject (35=9) when a cancel cannot be done.
  - Security List (35=y).
  - Business Message Reject (35=j) for unsupported messages, 35=G included.
- **Sequence numbers** persist in Postgres. On reconnect, missed execution reports are resent.
- **FIX version:** the builder picks the version (FIX 4.4 suggested, the version the OMS already speaks to Binance).

### 4.5 Kill, Enable, Flatten, disconnect

- **Kill** (cockpit button):
  1. sets the strategy Halted;
  2. cancels all its open orders through the OMS's own venue connections;
  3. logs its session out with a reason, and refuses new Logons until **Enable**;
  4. disables its REST trading token until **Enable**.

  **Kill never touches positions.** Enable sets the strategy back to Active.
- **Flatten** (separate button, with a confirmation):
  1. does everything Kill does, but leaves the strategy **Reducing** instead of Halted, so the closing orders pass the state check;
  2. then, for each non-zero position, sends a **limit order inside the price band** on the connection holding it. The limit is at the band's edge on the aggressive side, so a thin order book cannot fill it far away.

  Flatten orders go through the normal order path and checks under the strategy's portfolio. They are marked as Flatten orders, and the disconnect and restart cancels leave them working. The strategy stays Reducing until someone presses Kill or Enable.
- **Disconnect:** whenever a strategy's FIX session ends — Logout, dropped connection, missed heartbeat, or an OMS restart — the OMS **cancels all its open orders** except Flatten orders. This is always on. REST has no session, so a REST-only strategy relies on Kill.

### 4.6 REST

Documented for strategy developers, with no new auth: a strategy is a service principal with a trading token (the existing `/admin` trading-token flow), granted its own portfolio. Its REST orders are subject to the same strategy limits, routing resolution and state as its FIX orders. A strategy may use REST and FIX at the same time. ClOrdID uniqueness and FIX execution reports cover FIX orders only.

### 4.7 Cockpit

A **Strategies** page:
- **List:** name, state, whether the session is logged on.
- **Detail:** identity, CompIDs, password reset, IP allowlist, REST token, connections, limits with per-instrument overrides; Kill, Enable and Flatten buttons.

Orders, fills and positions stay on the existing Blotter and Positions pages, filtered to the strategy's portfolio. Mockup: `.superpowers/brainstorm/79842-1793891472/content/p7-strategies.html` (not committed).

## 5. My calls (open to challenge)

- **Hold timeout:** a held order whose instrument gets no quote within **2 s** is rejected (`no reference price`).
- **Instruments no feed covers at all** (today the equities, SPY and AAPL via Alpaca): the price band is skipped and logged. The size and position limits still apply.
- **Disconnect cancel** has no per-strategy switch.
- **After an OMS restart**, strategy orders still open at a venue are cancelled during startup recovery.
- **Flatten time in force** is GTC. Whatever stays unfilled is visible in the blotter for the user to handle.
- **Flatten for a position whose instrument has no mark** after the hold timeout: skipped and reported in the cockpit, never sent at market.
- **TLS everywhere**, internal network included.
- **At most one connection per broker code** per strategy.
- **Tag 100 is the routing (broker) code and tag 55 that broker's symbol.** This refines "venue symbol plus venue", which it equals for crypto venues.
- **Flatten leaves the strategy Reducing.** Its orders are exempt from the disconnect and restart cancels.
- **Kill disables the REST token.** Cancel-on-disconnect applies to FIX sessions only.
- **FIX execution reports cover FIX-originated orders only.**
- **Measuring market orders against the mark** applies to strategy orders only; cockpit orders are unchanged.

## 6. Left to the builder

- Table and column names; how per-strategy defaults and per-instrument overrides sit next to `risk_limits`.
- Where the acceptor lives: next to the order gateway in `crates/gateway`, or as an OMS module. It needs the DB for sequence numbers and the registry, so the likely home is the OMS side, using the gateway's FIX engine.
- Certificate and key configuration for the acceptor's TLS, and the listening port.
- How ClOrdID maps to the OMS order id: stored on the order, unique per strategy.
- The exact Security List fields.
- The wording of reject reasons. They must name the check and its numbers.
- How the test client for the "done when" is built: a small Rust or Python FIX initiator in the repo's tests or tools.
