---
title: Co-located strategies
description: Run a latency-sensitive strategy on its own server, trading the venue directly, while the OMS follows and controls it.
---

A co-located strategy trades its venue **directly**, from its own server next to the
exchange (or wherever it has the compute). It never waits for the OMS. The OMS follows
the strategy's venue account and shows its fills, positions and P&L moments after they
happen. It can stop the strategy at any time without the strategy's help.

This page covers Binance. Strategies that can afford a few milliseconds per order are
simpler to run [through the OMS](/strategies/fix/).

## Setup

The trader requests a co-located strategy; an admin sets it up:

1. At Binance, create the strategy's own account: a sub-account for live, a separate
   testnet account for paper. Never the account the OMS itself trades on.
2. Create two API keys on it, neither allowed to withdraw:
   - **the strategy's trading key**, restricted to the co-located server's IP, given to the trader;
   - **the OMS's follow key**, read and trade (to cancel), restricted to the OMS server's IP.
3. In the cockpit, on **Strategies**, create the strategy with mode **Co-located**:
   - the followed account's environment, the dead man's switch seconds (default 5) and
     the max orders per second;
   - its limits;
   - optionally **Kill when unmonitored**.
4. **Follow key:** enter the OMS's key. **Connections:** pick the strategy's Binance
   account in its portfolio. Create a trading token for the strategy's principal on the
   **API** page; the program uses it for the control link.

The OMS needs a `[control_link]` block in its configuration:

```toml
[control_link]
port = 9879
certificate = "/etc/oms/link.pem"
private_key = "/etc/oms/link-key.pem"
```

The certificate must name the host the program connects to (subject alternative name)
and allow server authentication. macOS clients also refuse certificates valid for more
than 825 days.

## What the OMS does

- **Books every fill** from Binance's own account stream, as an external order of the
  strategy's portfolio, so positions and P&L stay exact even if the program crashes. A
  fill that arrives twice is booked once. After a reconnect, the trade history fills
  any gap.
- **Books only fills.** Open orders and per-minute order counts are kept in memory and
  shown on the Strategies page.
- **Tags:** a client order id like `mm1-q-0001` shows its tag (`mm1-q`) on the fill.
- **Unknown pairs** are added to the catalog from Binance's exchange information, then booked.

## The control link

A WebSocket over TLS from the program to the OMS, with the strategy's trading token as
`Authorization: Bearer`. Messages are JSON with a `type` field.

| From the OMS | Meaning |
|---|---|
| `state` | Always first: `active`, `limits`, `limits_version`, `positions`, `dead_man_secs`. |
| `limits` | New limits and their version. |
| `positions` | The OMS's booked positions, whenever they change. |
| `kill` / `resume` | Stop / start again. |
| `heartbeat` | Every second. |

| From the program | Meaning |
|---|---|
| `ack_limits` | The `limits_version` it now enforces. |
| `ack_kill` | It stopped. |
| `heartbeat` | Every second. |

The `colo-link` crate implements the client. Its `DeadMan` switch says whether the
program may trade:
- it must have been told it is active;
- it must not be killed;
- it must have heard the OMS within the timeout.

The timeout is the strategy's dead man's switch seconds, sent in `state`. After it the
program stops quoting and cancels its own orders. It resumes by itself when the OMS is
back and says it is active. A kill holds until **Enable**; if Enable happened while the
link was down, the next `state` says active and lifts it.

The OMS drops a link it has not heard for 3 heartbeats; the program reconnects.

## The risk library

`crates/risk` is the single pure evaluator shared with the ordinary OMS path.
The embedded runtime (`crates/colo-runtime`) additionally owns reservations,
the durable local journal, recovery and the native-asset ledger. It runs every
order through, in memory:
- the strategy's limits: order and position size and notional;
- directional worst-case exposure, with opposing working orders never netted;
- a price band around the strategy's configured reference price;
- a max-orders-per-second cap;
- the halted/reducing state.

Notional defaults inherited into a non-USD-like quote are rejected; an explicit
per-instrument override may use any quote. The runtime adds account-wide Spot
asset reservations: BTC/USDT and ETH/USDT share the same USDT capacity, and fees
in a third asset live in their own ledger row without FX conversion.

## Control protocol

v2 uses two authenticated WSS connections. `/v2/control` carries bootstrap,
policy, authority, kill and enable; `/v2/events` carries bounded event batches
and durable ACKs. Both are bound to the same deployment, epoch and session
nonce. An event backlog therefore cannot delay a kill, and the event connection
cannot authorize trading. Receipt-time authority expiry includes time spent
queued; heartbeats do not renew it.

## Kill and alarms

- **Kill:**
  1. tells the program;
  2. cancels every open order on the account with the OMS's key at once, without waiting;
  3. while killed, cancels every new order the moment it appears, and sweeps the
     account for open orders every 2 seconds (**killed, still fighting** on the
     Strategies page). At most 10 cancels a second per strategy.
- **Kill not acknowledged** (orange badge): the program has not answered a kill within
  5 seconds.
- **Enable** resumes.
- **Unmonitored** (red badge): the strategy traded while its control link was down for
  longer than its dead man's switch seconds plus 2 (after an OMS restart too), or did
  not acknowledge the current limits within 10 seconds. With **Kill when
  unmonitored**, the OMS kills instead of only warning.
- **Order rate:** more orders in a second than its cap.

## Operating the runtime

On **Strategies**, **Runtime** shows what the strategy's runtime last reported:
- its epoch and the policy revision it has installed;
- a kill's progress: received, orders stopped, cancels sent, nothing resting;
- whether its recovery after a start completed, and what was missing if not;
- any **balance shortfall**.

**Balance checks.** The runtime keeps the balance it expects of every asset:
the opening balances, moved by each fill and each fee (a BNB fee counts against
BNB). Every 30 seconds it compares them with the venue. If an asset is short by
more than 10 bps (or two lot steps) three times in a row, it stops placing
orders, cancels nothing, and reports the shortfall. A balance higher than
expected never alarms, so deposits are harmless.

**Approve balances** (`POST /admin/strategies/{code}/rebaseline`) tells the
runtime to take the venue's current balances as expected, after a withdrawal,
a deposit or a manual trade. It lifts a balance stop.

**Restarting a runtime.** A new runtime process is a new owner. The OMS refuses
it until you confirm the old one is stopped: **Record verified stop**
(`POST /admin/strategies/{code}/deployments/{id}/verified-stop`, with how you
verified it). The replacement then takes over at the next epoch, which it
learns from the OMS. A kill stays in force across the takeover.

A submit whose answer was lost (a timeout) stops new orders until a lookup
settles it. Binance refuses a request that arrives more than 5 seconds after it
was signed, so once 10 seconds have passed, two "no such order" answers mean the
order never existed: it is closed and trading resumes.

## Try it on testnet

`scripts/colo-demo.sh` runs the whole setup on Binance Spot testnet with a
throwaway OMS: its own Postgres container, configuration, TLS certificate,
strategy, deployment and the reference market maker.

1. Create an Ed25519 key pair and register its public key at
   [testnet.binance.vision](https://testnet.binance.vision/):

   ```sh
   mkdir -p ~/.config/openoms/binance-testnet && cd ~/.config/openoms/binance-testnet
   openssl genpkey -algorithm ed25519 -out ed25519-private.pem
   openssl pkey -in ed25519-private.pem -pubout
   ```

   Save the API key Binance shows in `api-key` in the same folder.
2. From a checkout: `scripts/colo-demo.sh up`, then `scripts/colo-demo.sh mm`.
   The cockpit runs at `http://127.0.0.1:3021/cockpit/`; the admin password is
   in `colo-demo/adminpw`.
3. `scripts/colo-demo.sh down` stops everything and deletes the demo.

The demo uses one testnet account for both the OMS's own Binance connection and
the strategy; give a real deployment separate accounts.

## The reference program

`crates/colo-example` is a small market maker on one pair:
- it receives v2 bootstrap from the OMS and quotes through the embedded runtime;
- it runs every quote through the shared evaluator and deterministic reservations;
- it sends orders only while the runtime is Ready and authority is current.

```
BINANCE_API_KEY=… BINANCE_PRIVATE_KEY="$(cat key.pem)" \
  cargo run -p colo-example -- \
  --oms-url wss://oms-host:9879 --deployment <uuid> --token <trading token> \
  --account-id <uuid> --account-ref <subaccount> --ca link.pem \
  --journal-dir ./colo-journal --symbol BTCUSDT
```

The caller owns the token, CA and session nonce; `colo-runtime` receives only an
injected `RuntimeLink`. It builds the gateway without FIX
(`default-features = false`), so no C++ toolchain is needed on the co-located
server.
