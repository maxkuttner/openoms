---
title: Strategies over REST
description: Trade a strategy through the OMS's REST API, with the same checks as FIX.
---

Set the strategy up as in [Strategies over FIX](/strategies/fix/); REST needs no FIX
password or restart. Create a trading token for the strategy under **People → API** and
send it as `Authorization: Bearer <token>`. Orders are recorded as the strategy's.

- `GET /strategies/securities`: what the strategy may trade (`venue`, `symbol`,
  `instrument_id`).
- `POST /orders/submit`, `POST /orders/cancel`: place and cancel.
- `GET /orders`, `GET /orders/{id}/events`: order updates.

```json
{
  "order_id": "<new UUID>",
  "client_order_id": "arb-0001",
  "portfolio_id": "<strategy's portfolio>",
  "symbol": "BTCUSDT",
  "venue": "BINANCE",
  "side": "buy",
  "order_type": "limit",
  "time_in_force": "gtc",
  "limit_price": 61850,
  "quantity": 0.01
}
```

Rejections answer `422` with the reason; a killed or flattened strategy gets `403`.
REST has no session, so nothing is cancelled when your program stops: use Kill.
