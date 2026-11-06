---
title: Co-located strategies
description: Run a latency-sensitive strategy on its own server, trading the venue directly, while the OMS follows and controls it.
---

A co-located strategy trades its venue **directly**, from its own server next to the
exchange (or wherever it has the compute). It never waits for the OMS. The OMS follows
the strategy's venue account and shows its fills, positions and P&L moments after they
happen. It can stop the strategy at any time without the strategy's help.

This page covers Binance. Strategies that can afford a few milliseconds per order are
simpler to run [through the OMS](/openoms/strategies/fix/).

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
| `state` | Always first: `active`, `limits`, `limits_version`, `positions`. |
| `limits` | New limits and their version. |
| `positions` | The OMS's booked positions, after each fill. |
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

After the timeout the program stops quoting and cancels its own orders. It resumes by
itself when the OMS is back and says it is active; a kill holds until **Enable**.

## The risk library

The `colo-risk` crate runs every order through, in memory:
- the strategy's limits: order and position size and notional;
- a price band around the program's own mid;
- a max-orders-per-second cap;
- the halted state.

Notional limits apply to pairs quoted in USD, USDT or USDC only.

## Kill and alarms

- **Kill:**
  1. tells the program;
  2. cancels every open order on the account with the OMS's key at once, without waiting;
  3. while killed, cancels every new order the moment it appears (**killed, still
     fighting** on the Strategies page).
- **Enable** resumes.
- **Unmonitored** (red badge): the strategy traded while its control link was down, or
  did not acknowledge the current limits within 10 seconds. With **Kill when
  unmonitored**, the OMS kills instead of only warning.
- **Order rate:** more orders in a second than its cap.

## The reference program

`crates/colo-example` is a small market maker on one pair:
- it quotes a bid and an ask around the mid from Binance's public feed;
- it runs every quote through the risk library, under the control link.

```
BINANCE_API_KEY=… BINANCE_PRIVATE_KEY="$(cat key.pem)" \
  cargo run -p colo-example -- --link wss://oms-host:9879 --token <trading token> \
  --ca link.pem --symbol BTCUSDT
```

It builds the gateway without FIX (`default-features = false`), so no C++ toolchain is
needed on the co-located server.
