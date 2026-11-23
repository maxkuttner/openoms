---
title: Strategies over FIX
description: Connect a strategy to the OMS over FIX 4.4 and trade through its checks.
---

A strategy can send orders to the OMS over FIX 4.4. Every order passes the same
pre-trade checks as one from the web app and shows up under the strategy's portfolio.
The OMS can stop the strategy at any time. Each order adds about 1–5 ms.

## Set up

In the Admin Panel, **Trading setup → Strategies**, create a strategy:

- **Portfolio:** only the strategy trades it.
- **Responsible user:** accountable for the strategy; changed only by **Handover**.
- **SenderCompID / TargetCompID**, optional **allowed IP addresses**.
- **Limits:** max order and position quantity and notional, price band %.

Then **Reset password** (shown once), pick its accounts under **Connections** (one per
broker), and **Restart FIX acceptor**. New strategies, CompIDs or IPs need a restart;
passwords, limits and state apply at once.

```toml
[fix_acceptor]
port = 9878
certificate = "/etc/oms/fix.pem"
private_key = "/etc/oms/fix-key.pem"
```

## Session

- TLS always. Logon (35=A) with your CompIDs, the password in 554, and `141=Y`.
- Logon is refused for a wrong password, an address not allowed, a killed strategy, or
  a second session. The reason shows on the Strategies page.
- After a reconnect the OMS resends the execution reports you missed.

## Messages

| You send | Tags |
|---|---|
| New Order Single (35=D) | 11 ClOrdID, 55 Symbol (broker's), 100 ExDestination (broker code), 54 Side, 38 OrderQty, 40 OrdType (1 market, 2 limit), 44 Price, 59 TimeInForce (0 day, 1 GTC, 3 IOC) |
| Order Cancel Request (35=F) | 11 ClOrdID, 41 OrigClOrdID |
| Order Status Request (35=H) | 11 ClOrdID |
| Security List Request (35=x) | 320 SecurityReqID |

```
35=D|11=arb-0001|55=BTCUSDT|100=BINANCE|54=1|38=0.01|40=2|44=61850|59=1
```

You receive Execution Reports (35=8) for every step, Order Cancel Rejects (35=9), the
Security List (35=y) and Business Message Rejects (35=j) for anything unsupported.
Amending (35=G) is not supported: cancel and resend.

## Pre-trade checks

- A killed strategy can place nothing; a flattened one only reducing orders.
- The strategy's limits are the default; a Risk limits row for one instrument overrides.
- Strategy-wide notional limits apply only to USD, USDT and USDC quotes; other quotes
  need a per-instrument limit.
- A limit price outside the price band around the live mark is rejected. With no mark
  after 2 seconds, the order is rejected (`no reference price`).

A rejection is an Execution Report with 150=8 and the reason in 58.

## Kill and flatten

- **Kill:** cancels every open order, logs the session out, refuses logons until
  **Enable**. Positions stay.
- **Flatten:** as Kill, then closes every position with a limit order at the edge of
  the price band.
- When your session ends for any reason, the OMS cancels your open orders.
