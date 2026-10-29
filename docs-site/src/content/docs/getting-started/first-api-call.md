---
title: Your first API call
description: Authenticate with a trading token and drive orders from curl or Python.
---

Everything a trader does goes through the trading API on the same port as the cockpit.
This page assumes you have a trading token from
[your first paper trade](/openoms/getting-started/first-paper-trade/) in
`OMS_TRADING_TOKEN`.

## Authentication

Send the token as a bearer header:

```sh
curl -s localhost:3001/portfolios -H "Authorization: Bearer $OMS_TRADING_TOKEN"
```

```json
[{"portfolio_id":"4e33bc9e-52e0-4dc5-87a7-356d59ffc681","code":"main","name":"Main","status":"ACTIVE","base_currency":"USD","can_trade":true,"can_view":true,"can_allocate":false}]
```

A token belongs to a principal. What it can see and trade comes from that principal's
portfolio grants — `can_view`, `can_trade`, `can_allocate` per portfolio. It can never
reach `/admin`. Mint several tokens per principal to rotate them without re-permissioning.

The cockpit password is a different credential: it is for `/admin` and the cockpit only.

## The calls you will use most

| Call | What it does |
|---|---|
| [`GET /portfolios`](/openoms/api/operations/list_my_portfolios/) | Portfolios this token may act on |
| [`GET /instruments?search=AAPL`](/openoms/api/operations/list_instruments_for_trader/) | Search the catalog, best match first |
| [`POST /orders/submit`](/openoms/api/operations/orders_submit/) | Place an order |
| [`GET /orders`](/openoms/api/operations/list_orders/) | Your blotter, newest first |
| [`GET /orders/{id}`](/openoms/api/operations/get_order/) | One order's current state |
| [`GET /orders/{id}/events`](/openoms/api/operations/get_order_events/) | One order's full history |
| [`POST /orders/cancel`](/openoms/api/operations/orders_cancel/) | Cancel an order |
| [`GET /portfolios/{id}/positions`](/openoms/api/operations/get_portfolio_positions/) | Positions in a portfolio |

Every endpoint, with request and response schemas, is in the
[API reference](/openoms/api/). A running server also serves its own spec at
`/api-docs/openapi.json` and an interactive explorer at `/scalar`.

### Search, then submit

```sh
T="Authorization: Bearer $OMS_TRADING_TOKEN"

curl -s "localhost:3001/instruments?search=AAPL&limit=2" -H "$T"
```

```json
[{"id":14074,"symbol":"AAPL","name":"Apple Inc. Common Stock","venue":"XNAS","asset_class":"EQUITY","instrument_class":"SPOT","status":"ACTIVE","has_options":false},
 {"id":11340,"symbol":"AAPB","name":"GraniteShares ETF Trust GraniteShares 2x Long AAPL Daily ETF","venue":"XNAS","asset_class":"EQUITY","instrument_class":"SPOT","status":"ACTIVE","has_options":false}]
```

An order names its instrument either by `instrument_id` or by `symbol` (add
`"venue": "XNAS"`, or write `AAPL@XNAS`, when a symbol exists on more than one venue).
`limit_price` is required for `"order_type": "limit"`.

### The blotter

```sh
curl -s "localhost:3001/orders?limit=5" -H "$T"
```

```json
[{"order_id":"36276e25-…","client_order_id":"first-trade","principal_code":"alice","portfolio_code":"main","broker_connection_code":"alpaca-paper","instrument_symbol":"AAPL","instrument_name":"Apple Inc. Common Stock","side":"buy","order_type":"market","status":"canceled","original_qty":1.0,"leaves_qty":1.0,"cum_qty":0.0,"avg_px":null,"created_at":"2026-10-29T06:32:01.923151Z","updated_at":"2026-10-29T06:32:02.050697Z"}]
```

## Python

The Python client wraps the same API:

```sh
pip install "git+https://github.com/maxkuttner/openoms#subdirectory=clients/python"
```

```python
import os
import time

from oms_client import OMS

oms = OMS("http://localhost:3001", token=os.environ["OMS_TRADING_TOKEN"])

pf = oms.portfolios()[0]
oid = oms.submit(portfolio=pf.portfolio_id, symbol="AAPL", side="buy", quantity=1)
time.sleep(1)
print("submitted:", oms.order(oid).status)

print("cancel:", oms.cancel(oid))
time.sleep(2)
print("now:", oms.order(oid).status)
```

```text
submitted: routed
cancel: pending
now: canceled
```

`oms.cancel()` returns `"pending"` when the cancel was forwarded to the broker (confirmed
asynchronously) and `"canceled"` when the OMS cancelled the order outright.
`oms.wait_for(oid)` blocks until an order reaches a final status — `filled`, `canceled`,
`rejected` or `expired` — and raises `TimeoutError` after 60 seconds otherwise, which is
what happens to an unfilled order outside market hours.

:::caution[Two commands called `oms`]
The Python package also installs a command-line tool named `oms` (a terminal blotter:
`oms orders`, `oms submit`, …). Inside a virtualenv where the client is installed, it
shadows the server binary. Install the client in its own virtualenv, and call the server
binary by its full path (`~/.local/bin/oms`) from there if you need both.
:::
