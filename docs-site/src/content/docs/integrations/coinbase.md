---
title: Coinbase
description: Route crypto spot orders to Coinbase Advanced Trade, live only.
---

Coinbase spot, for crypto, through Coinbase's Advanced Trade API. The OMS routes orders
over REST, receives order updates on Coinbase's private user stream, and prices Coinbase
positions from Coinbase's public ticker.

:::caution[Live only]
Coinbase has no paper trading, so there is only a `coinbase-live` connection and
**every order is real money**. Start with small orders.
:::

:::note
This integration is tested against Coinbase's documented API responses, but has not yet
been verified end to end with a real Coinbase account.
:::

## What it covers

| | |
|---|---|
| Order entry | REST: market and limit orders, cancel |
| Executions | Coinbase's private user stream: fills, partial fills, cancels, rejects, expiries |
| Instruments | Spot products (`BTC-USD`, `ETH-EUR`, …) |
| Catalog | Broker connections → **Instrument universe** in the cockpit |
| Prices | Coinbase's public ticker |

## Credentials

Coinbase uses a **CDP API key**. In the
[Coinbase Developer Platform](https://portal.cdp.coinbase.com/projects/api-keys), create a
secret API key for Advanced Trade with:

- the **ECDSA** signature algorithm (Ed25519 keys are not supported);
- the **View** and **Trade** permissions only. Leave **Transfer** off: the OMS never
  needs to move money out, and **Test** refuses a key that could.

Coinbase shows the key once, as a key name and a private key. The broker connection
`coinbase-live` is created at startup; open its credentials drawer and fill in:

| Field | What it is | Where to get it |
|---|---|---|
| API key | The key's name, `organizations/…/apiKeys/…` | Shown when you create the key |
| Private key (PEM) | The key's private key, starting `-----BEGIN EC PRIVATE KEY-----`; pasting it with `\n` escapes, as it appears in the downloaded JSON, works too | Shown once, when you create the key |

**Test** checks the key against Coinbase and its permissions. **Save** applies it right
away, with no restart: the Coinbase stream appears under **Live execution streams** once
it connects.

## Loading instruments

Saving the credentials offers to load Coinbase's spot products as a background job; the
**Instrument universe** column on **Broker connections** shows its state and has
**Load** / **Reload**. From a terminal: `oms setup sync-broker --broker coinbase --no-enrich`.

Coinbase products are their own instruments: `BTC-USD` on Coinbase is a different entry
from `BTCUSDT` on Binance. To trade on Coinbase, the order's account must be linked to the
`coinbase-live` connection.

Coinbase quotes products in USD, USDC, USDT, EUR, GBP and a few other currencies, but not
CHF; trade the pairs your account holds a balance in.

## Limits

- Spot only: no futures or perpetuals.
- Market orders are sized in the base coin (for example 0.0001 BTC).
- Orders cannot be amended; cancel and place a new one.
- Fees: booked from the running fee total Coinbase reports per order; see [Fees](/concepts/fees/).
