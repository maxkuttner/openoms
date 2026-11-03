---
title: Bybit
description: Route crypto spot orders to Bybit, testnet or live.
---

Bybit spot, for crypto. The OMS routes orders to Bybit over its v5 REST API, receives
fills on Bybit's private WebSocket, and marks crypto positions from Bybit's public order
book.

:::note
This integration is tested against Bybit's documented API responses, but has not yet
been verified end to end with a real Bybit account.
:::

## What it covers

| | |
|---|---|
| Order entry | REST: market and limit orders, cancel |
| Executions | Bybit's private order and execution stream: fills, partial fills, cancels, rejects |
| Instruments | Spot pairs |
| Catalog | Broker connections → **Instrument universe** in the cockpit |
| Prices | Bybit's public spot book; also a backup source for Binance pairs |

## Credentials

Bybit uses an **HMAC API key**. For PAPER, sign up at the
[Bybit testnet](https://testnet.bybit.com/) and open
[API management](https://testnet.bybit.com/app/user/api-management); for LIVE, use
[API management](https://www.bybit.com/app/user/api-management) on bybit.com. Create a
**System-generated** key with **Read-Write** permission for spot trading.

Create a broker connection with **Bybit** as the venue; its credentials drawer opens
with these fields:

| Field | What it is | Where to get it |
|---|---|---|
| API key | Identifies your key | Shown in API management after you create the key |
| API secret | The key's secret | Shown once, when you create the key; if you lost it, create a new key |

**Test** checks the key against Bybit. **Save** applies it right away, with no restart:
the Bybit stream appears under **Live execution streams** once it connects.

## Loading instruments

Saving the credentials offers to load Bybit's spot pairs as a background job; the
**Instrument universe** column on **Broker connections** shows its state and has
**Load** / **Reload**. From a terminal: `oms setup sync-broker --broker bybit --no-enrich`.

Bybit pairs are their own instruments: `BTCUSDT` on Bybit and `BTCUSDT` on Binance are
two entries in the instrument picker, one per venue. To trade on Bybit, the order's
account must be linked to the `bybit-paper` (or `bybit-live`) connection.

## Paper or live

| Variable | Values | Default |
|---|---|---|
| `BYBIT_ENV` | `PAPER` (testnet) or `LIVE` | `PAPER` |

`BYBIT_ENV` picks which connection the instrument load uses; the connection itself
(`bybit-paper` or `bybit-live`) decides where its orders go.

## Prices

The OMS subscribes to Bybit's public spot top-of-book (no account, no key) and uses it to
mark Bybit positions. For pairs held on Binance it is a backup, used when Binance's feed
is down.

## Limits

- Spot only: no perpetuals, futures, options or margin.
- No FIX; Bybit offers FIX to institutional clients only.
- Orders cannot be amended; cancel and place a new one.
