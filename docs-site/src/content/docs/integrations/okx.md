---
title: OKX
description: Route crypto spot orders to OKX, demo trading or live.
---

OKX spot, for crypto. The OMS routes orders to OKX over its v5 REST API, receives
fills on OKX's private WebSocket, and marks OKX positions from OKX's public tickers.

:::note
This integration is tested against OKX's documented API responses, but has not yet
been verified end to end with a real OKX account.
:::

## What it covers

| | |
|---|---|
| Order entry | REST: market and limit orders (GTC, IOC, FOK), cancel |
| Executions | OKX's private orders stream: fills, partial fills, cancels |
| Instruments | Spot pairs |
| Catalog | Broker connections → **Instrument universe** in the cockpit |
| Prices | OKX's public tickers, for OKX pairs |

## Credentials

OKX uses an **API key with a passphrase**. PAPER is OKX **Demo Trading**: log in to
okx.com, switch to Demo Trading, and create a demo API key there (demo keys only work
in Demo Trading). For LIVE, create a key under
[API](https://www.okx.com/account/my-api) in your normal account. Give the key the
**Read** and **Trade** permissions only; a key that can **Withdraw** fails **Test**.

Create a broker connection with **OKX** as the venue; its credentials drawer opens with
these fields:

| Field | What it is | Where to get it |
|---|---|---|
| API key | Identifies your key | Shown on the API page after you create the key |
| API secret | The key's secret | Shown once, when you create the key; if you lost it, create a new key |
| Passphrase | The passphrase you typed when creating the key | You chose it; OKX cannot show it again |

**Test** checks the key's permissions against OKX. **Save** applies it right away, with
no restart: the OKX stream appears under **Live execution streams** once it connects.

## Loading instruments

Saving the credentials offers to load OKX's spot pairs as a background job; the
**Instrument universe** column on **Broker connections** shows its state and has
**Load** / **Reload**. From a terminal: `oms setup sync-broker --broker okx --no-enrich`.

OKX pairs are their own instruments: `BTC-USDT` on OKX and `BTCUSDT` on Binance are two
entries in the instrument picker, one per venue. To trade on OKX, the order's account
must be linked to the `okx-paper` (or `okx-live`) connection.

## Paper or live

| Variable | Values | Default |
|---|---|---|
| `OKX_ENV` | `PAPER` (Demo Trading) or `LIVE` | `PAPER` |

`OKX_ENV` picks which connection the instrument load uses; the connection itself
(`okx-paper` or `okx-live`) decides where its orders go.

## Prices

The OMS subscribes to OKX's public tickers (no account, no key) and uses them to mark
OKX positions. Demo Trading positions are marked at live OKX prices.

## Limits

- Spot only, cash mode: no margin, perpetuals, futures or options.
- Accounts registered in the US, EEA or Australia use separate OKX hosts, which the
  OMS does not support yet.
- Orders cannot be amended; cancel and place a new one.
- Limit prices are sent as entered; OKX rejects a price off the pair's tick size.
- Fees: booked from the running fee total OKX reports per order; see [Fees](/concepts/fees/).
