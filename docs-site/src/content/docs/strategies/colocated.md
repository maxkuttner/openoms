---
title: Co-located strategies
description: Legacy. A strategy on its own server trades Binance directly; the OMS follows its account and can stop it.
---

> **Legacy:** being replaced by [reported strategies](/strategies/reported/). Use those for new strategies.

A co-located strategy trades Binance **directly** from its own server and never waits for
the OMS. The OMS follows the strategy's account with its own key, books every fill into
the strategy's portfolio, and can cancel everything on the account without the
strategy's help.

## Setup

1. At Binance, give the strategy its own account (a sub-account, or a separate testnet
   account). Create two keys on it, neither allowed to withdraw: the **strategy's
   trading key** and the **OMS's follow key** (read and trade, to cancel).
2. In the Admin Panel, **Trading setup → Strategies**, create the strategy with mode
   **Co-located**: environment, dead man's switch seconds (default 5), max orders per
   second, limits, optionally **Kill when unmonitored**. Enter the follow key and pick
   the Binance account under **Connections**.
3. Create a trading token for the strategy on the **API** page; the program uses it for
   the control link.
4. Add the control link to the OMS configuration. The certificate must name the host
   the program connects to.

```toml
[control_link]
port = 9879
certificate = "/etc/oms/link.pem"
private_key = "/etc/oms/link-key.pem"
```

## Kill and alarms

- **Kill** tells the program, cancels every open order on the account with the OMS's
  key, and keeps cancelling new orders until **Enable**.
- **Kill not acknowledged:** the program did not answer within 5 seconds.
- **Unmonitored:** the strategy traded while its control link was down longer than its
  dead man's switch, or did not acknowledge new limits within 10 seconds. With **Kill
  when unmonitored**, the OMS kills instead of warning.
- The program stops quoting by itself when it loses the OMS for longer than its dead
  man's switch.

## Reference program

`crates/colo-example` is a small market maker built on `colo-runtime`.
`scripts/colo-demo.sh up` then `scripts/colo-demo.sh mm` runs it against Binance Spot
testnet with a throwaway OMS; `scripts/colo-demo.sh down` removes it.
