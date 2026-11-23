---
title: API
description: Trade programmatically with a personal API key.
---

Open **Trade → API** and create a key. The secret is shown **once**. A key acts as you,
with your portfolio access; revoke it there at any time.

Send it as `Authorization: Bearer <key>`:

- `POST /orders/submit`, `POST /orders/cancel`: place and cancel.
- `GET /orders`, `GET /orders/{id}/events`: orders and their history.

```json
{
  "order_id": "<new UUID>",
  "client_order_id": "my-0001",
  "portfolio_id": "<portfolio>",
  "symbol": "BTCUSDT",
  "venue": "BINANCE",
  "side": "buy",
  "order_type": "limit",
  "time_in_force": "gtc",
  "limit_price": 61850,
  "quantity": 0.01
}
```

Orders pass the same pre-trade checks as the web app; a rejection answers `422` with the
reason. Every endpoint is in the [API reference](/api/).
