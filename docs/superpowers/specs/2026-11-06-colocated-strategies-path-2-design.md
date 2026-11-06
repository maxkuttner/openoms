# Strategies / algos path ②: co-located strategies

Date: 2026-11-06. Status: design, agreed in conversation; not started.

Parent: `docs/superpowers/specs/2026-11-04-target-architecture-design.md` (branch `docs/target-architecture`), §4.4 path ② and §7 phase 8. That document asks for this design before phase 8 is built.

Builds on:
- phase 7, the strategy registry and FIX/REST strategies (`docs/superpowers/specs/2026-11-05-strategies-path-1-design.md`);
- the order engine gaps work (branch `feat/engine-gaps`).

## 1. Intent

Latency-sensitive strategies, market making first, run on their own servers: next to the exchange, or wherever there is enough compute. They trade **directly with the venue**, with no hop through the OMS.

The OMS is an **almost real-time, after-the-fact view** of what such a strategy does:
- its fills, positions and P&L appear moments after they happen;
- the OMS can stop it at any time, without the strategy's cooperation.

Traders and developers are never forced through the OMS's order path to run HFT strategies.

## 2. Goals, non-goals, anti-goals

**Goals**
1. **Done when** (parent §7): a sample co-located program trades on a paper venue (Binance testnet), its fills appear in the OMS positions, and Kill stops it.
2. **Fills come from the venue, not the strategy.** Every fill on a co-located strategy's account is booked by the order engine, from the venue's own account stream, even if the strategy crashes or under-reports.
3. **Kill works without the strategy.** The OMS cancels everything on the account with its own key, and also tells the strategy.
4. **The strategy stops itself** when it loses contact with the OMS.
5. **A Rust risk library** gives co-located programs the same pre-trade limits the OMS applies, plus an orders-per-second cap.

**Non-goals**
- On-chain venues (Uniswap and similar). They are a later phase of their own.
- The parent design's Kafka activity topic (`strategy.<id>.activity`) and the reference outbox forwarder. Strategies report nothing beyond what the venue reports; signals stay in their own logs or database.
- Kafka control topics. Control runs over a WebSocket to the OMS (4.4).
- Python or C++ bindings for the risk library.
- Venues other than Binance in this phase.
- Firm-wide limits and end-of-day reconciliation. They are on the "later" list (§7).

**Anti-goals: these would be failures even if they technically work**
- A fill on the strategy's account that the OMS never books.
- Kill depends on the strategy behaving.
- The OMS's order path or database slows down because a market maker places and cancels thousands of orders.
- The strategy's latency depends on the OMS: any synchronous call to the OMS on its order path.
- A strategy trading without anyone noticing that it is not connected, or not running the current limits.

## 3. Constraints

- **Repo rules:**
  - Rust workspace; Postgres is the system of record; migrations under `db/migrations/ods/oms/`.
  - Single user, pre-release: no compatibility shims.
  - No help text in the UI; explanations go in the docs site.
  - DB tests are `#[ignore]`; never create databases on the shared dev Postgres.
- **What the OMS may book:** the order engine is the only place that books (parent §4.1). External fills go through it as `ExecEvent`s.
- **Keys:** keys are created at the venue by an admin, never by traders. Neither key can withdraw, and each is IP-restricted to its own machine.

## 4. What was decided

### 4.1 Setup: who creates what

The trader, or whoever they report to, requests a co-located strategy. An admin then:
1. creates the strategy's **account at the venue**:
   - live: a Binance sub-account per strategy;
   - testnet, which has no sub-accounts: a separate testnet account, through its own login;
2. creates **two API keys** on that account:
   - the **strategy's trading key**, given to the trader and usable only from the co-located server;
   - the **OMS's key**, read plus trade (needed to cancel), usable only from the OMS server, and stored encrypted in the OMS. It is a **follow-only credential** of the strategy, not a broker connection:
     - the OMS uses it only to follow the account and to cancel;
     - it never places orders there;
     - so it needs none of the per-(broker, environment) connection machinery.
3. registers the strategy in the cockpit as a phase 7 strategy with mode **co-located**.

**Optionally**, where the venue's API allows it, a master-account key in the OMS lets Kill also switch off the strategy's own key. This is opt-in per venue.

### 4.2 Following the account

- **One connection per account.** For each co-located strategy, the OMS runs Binance's account stream for the strategy's account, with the OMS's key. It reuses the Binance connector's stream code, once per followed account.
- **External orders.** An order on that account that the OMS did not place is an **external order** of the strategy's portfolio, booked through the order engine. Today the engine skips a fill for an unknown order (`execution.rs` TODO); that gap closes here for followed accounts.
- **Fills only.** The OMS books **only fills**: each fill creates or extends an external order and moves positions and P&L.
  - Open orders are held **in memory only**, from the stream: a count plus the most recent ones, for the cockpit and for Kill.
  - Per-minute counts of placed, cancelled and rejected orders feed the alarms.
- **Unknown pairs.** A fill on a pair not in the OMS's catalog is never left unbooked. The OMS looks the pair up in Binance's exchange information, adds the instrument and its broker mapping to the catalog, then books the fill.
- **Order tags.** The strategy may put a short tag in each order's client id, for example `mm1-q-0001` (quote) or `mm1-h-0002` (hedge). The OMS reads the tag from Binance's reports and shows it on fills in the blotter.
- **Gaps.** After a stream reconnect, the OMS compares recent trades with what it booked and books what it missed (recon, as for other connectors), so a dropped stream loses no fills.

### 4.3 The risk library (new crate)

The library is pure and in memory, with no I/O, and is called by the co-located program before every order. Its checks:
- the strategy's limits, as in phase 7: max order quantity and notional, max position quantity and notional, the price band around the strategy's own reference price;
- a **max orders per second** cap, set per strategy in the cockpit;
- the stopped/killed state: no new orders.

The program gets its limits from the control link (4.4) with a version number, and enforces them all in memory. It checks position limits against the **positions the OMS has booked**: the link sends them on connect and on every change.

### 4.4 The control link (new crate, plus an OMS endpoint)

- **Transport:** a WebSocket over TLS on its own port, configured like the FIX acceptor (certificate, key, and the strategy's IP allowlist). The OMS's HTTP server has no TLS. It is authenticated with the strategy's trading token (phase 7's REST token), and Kill does not cut it: a killed strategy can still acknowledge and hear `resume`.
- **OMS → strategy:**
  - the current limits with their version;
  - `kill` and `resume`;
  - its booked positions.

  On every connect, the OMS first sends the full current state: active or killed, the current limits, and the positions.
- **Strategy → OMS:**
  - the acknowledgement of each limits version;
  - the acknowledgement of a kill;
  - heartbeats (both ways, every 1 s).
- **Dead man's switch.** Without the OMS for **5 s** (configurable per strategy), the strategy stops quoting:
  - it sends no new orders and cancels its own open orders;
  - it never touches positions.

  It resumes by itself when it reconnects and the OMS says it is active. A killed strategy stays stopped until Enable in the cockpit.
- **Off the order path.** The link is a separate, quiet connection. The strategy never calls the OMS while placing an order.

### 4.5 Kill, Enable and alarms

- **Kill** (cockpit), all at once:
  1. send `kill` down the link;
  2. cancel every open order on the account with the OMS's key, without waiting for the strategy;
  3. where configured, switch off the strategy's key.

  The acknowledgement is shown when it arrives; no acknowledgement within 5 s is flagged. Positions are not touched.
- **While killed, the OMS keeps fighting.** It cancels every new order it sees on the account the moment it appears, with its own key, until Enable. The cockpit shows **killed, still fighting** while orders keep appearing. Cancels respect Binance's rate limits.
- **States:** a co-located strategy is only ever Active or Killed (no Reducing), and has no Flatten.
- **Phase 7's cancels do not apply:** neither cancel-on-disconnect nor the startup cancel. The strategy's own dead man's switch does that job, and an OMS restart never cancels its quotes.
- **Enable** sends `resume` (and switches the key back on, where it was switched off).
- **"Unmonitored" alarm.** An order or fill appears on the account while either:
  - the strategy has no control link; or
  - it has not acknowledged the current limits version within 10 s of a change.

  Then the cockpit shows a red **unmonitored** badge and the OMS logs `error!`. A per-strategy switch, **kill when unmonitored**, off by default, makes the OMS kill instead.
- **Orders-per-second alarm.** The followed account's placed-order count exceeds the strategy's cap. This is the same badge, as a separate reason.

### 4.6 The reference program (new, in this repo)

A small Rust market maker on Binance testnet. It:
- quotes one bid and one ask around the market price on one pair;
- trades through the gateway crate's Binance connector and reads prices from its public feed;
- runs every order through the risk library;
- holds the control link with the dead man's switch;
- tags its orders.

It is the reference for strategy developers, and the test of the "done when".

### 4.7 Cockpit

For a co-located strategy, the Strategies page also shows:
- its mode;
- the followed account's connection (the OMS's key);
- the control link (connected, last heartbeat);
- the limits version set against the version acknowledged;
- the dead-man timeout;
- max orders per second;
- the open-order count and order-flow counts;
- the unmonitored badge.

Fills appear in the blotter with their tag, and positions in the strategy's portfolio.

## 5. My calls (open to challenge)

- **Two separate new crates:** the risk library (pure checks) and the control-link client. Risk checks are usable with no network code.
- **FIX behind an optional feature.** The gateway crate's FIX engine (C++/OpenSSL) moves behind a cargo feature, so a co-located build stays light.
- **Defaults:** dead-man timeout 5 s; heartbeat 1 s; limits must be acknowledged within 10 s; a kill acknowledgement is expected within 5 s.
- **Unmonitored is an alarm by default,** not a kill.
- **Tags** come from the client order id's prefix, up to the second `-` (`mm1-q`).
- **The reference program is a market maker on one pair,** in this repo.
- **On testnet, a separate account** for the strategy rather than sharing the OMS's paper account.
- **Follow-only credential** per strategy, not a broker connection; the OMS never places orders on a co-located account.
- **Only Active and Killed;** no Reducing, no Flatten, none of phase 7's automatic cancels; Kill keeps the control link.
- **Positions go down the link,** so the risk library checks the OMS's booked numbers.
- **The control link has its own TLS port.**

## 6. Left to the builder

- Crate and module names and layout; the WebSocket message format (JSON suggested).
- How "external order" is represented: a synthetic `order_state` row per venue order id, created at its first fill, is the likely shape.
- Where the per-account streams live (next to the connector's own stream), and how they are started and stopped with the registry.
- The exact trade-history query Binance offers for gap recon.
- How the in-memory open-order view is exposed to the cockpit.
- Which master-account call, if any, switches a Binance sub-account key off. If none exists, the opt-in is simply unavailable on Binance.

## 7. Later (to-do)

- **Firm-wide limits:** exposure per asset across strategies and venues, capital used, daily loss. These need an FX source.
- **End-of-day reconciliation** of positions against venue statements, outside the OMS (the book-of-record concern).
- **On-chain venues** (Uniswap and similar): wallet following, liquidity positions, on-chain kill.
- **Python and C++ bindings** for the risk library.
- **More venues:** Bybit and OKX sub-accounts, and exchange drop-copy sessions.
- **Booking every order** (placements and cancels, not only fills) for low-volume co-located strategies, as a per-strategy switch.
