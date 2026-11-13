---
title: Strategies over REST
description: Trade a strategy through the OMS's REST API, with the same checks as FIX.
---

A strategy can use the REST API instead of FIX, or alongside it. Set the strategy up as
described in [Strategies over FIX](/strategies/fix/); the REST side needs no FIX
password, port or restart.

## Token

On the cockpit's **API** page, create a trading token for the strategy. The token
belongs to the strategy, not to its responsible user, and orders sent with it are
recorded as the strategy's, with the responsible user noted for audit. Send it as `Authorization: Bearer <token>`.

## What you may trade

```
GET /strategies/securities
```

returns every instrument the strategy may trade, as `venue` (the broker code), `symbol`
(the broker's symbol) and `instrument_id`.

## Placing and cancelling

```
POST /orders/submit
{
  "order_id": "<a new UUID>",
  "client_order_id": "arb-0001",
  "portfolio_id": "<the strategy's portfolio>",
  "symbol": "BTCUSDT",
  "venue": "BINANCE",
  "side": "buy",
  "order_type": "limit",
  "time_in_force": "gtc",
  "limit_price": 61850,
  "quantity": 0.01
}
```

`venue` is the broker code and `symbol` that broker's symbol, as in the securities list;
the strategy's account for that broker is used. Cancel with `POST /orders/cancel`.

The same pre-trade checks apply as over FIX, and a rejection answers `422` with the
reason. A killed or flattened strategy's token is refused (`403 strategy … is stopped`)
until it is enabled again. REST has no session, so nothing is cancelled when your program
stops: use Kill.

Order updates are on `GET /orders` and `GET /orders/{id}/events`. Execution reports over
FIX cover only orders sent over FIX.
