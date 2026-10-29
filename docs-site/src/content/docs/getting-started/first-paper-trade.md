---
title: Your first paper trade
description: Connect Alpaca paper, load the catalog, set up a trader and send 1 share of AAPL.
---

This guide takes a fresh install to a real order at Alpaca's paper-trading venue: one
share of AAPL. No market-data subscription is needed. It assumes the server is running
on `localhost:3001` ([First run](/openoms/getting-started/first-run/)).

## 1. Get Alpaca paper keys

Create a free account at [alpaca.markets](https://alpaca.markets), switch to **Paper
Trading**, and generate an API key. You get a key ID and a secret.

## 2. Store the credentials

In the cockpit, open **Broker connections → alpaca-paper → Credentials**, enter the key
and secret, click **Test**, then **Save**. They are encrypted with the master key in
`oms.toml` and stored in Postgres — never in a file.

Then make the running server pick them up without a restart:

```sh
export OMS_ADMIN=$(sed -n 's/^admin_password = "\(.*\)"/\1/p' ~/.config/oms/oms.toml)

curl -s -X POST localhost:3001/admin/connections/reload -H "Authorization: Bearer $OMS_ADMIN"
```

```json
{"connections":[["alpaca-paper","Registered"],["binance-paper","Unconfigured"],["databento-opra","Unconfigured"]]}
```

`Registered` means the adapter is live. The other connections stay `Unconfigured` until
you give them credentials.

## 3. Load the instrument catalog

The catalog comes from your broker, not a bundled list:

```sh
oms setup sync-broker --broker alpaca --no-enrich
```

```text
fetched 14404 tradeable instrument(s) from ALPACA
sync-broker done: upserted 14404 ALPACA broker_instrument row(s)
```

This takes a few seconds. Without `--no-enrich` it also looks up a FIGI for every symbol
through OpenFIGI, which is rate-limited and takes a long time without an
`OPENFIGI_API_KEY` — skip it for now. With credentials stored, an empty catalog also
syncs automatically when the server starts.

## 4. Set up a trader

Orders are placed by a **principal** (a person, strategy or service) into a
**portfolio**, which books through an **account** at a broker. A **trading token** lets
the principal call the trading API. Create one of each with the admin API (the cockpit
has the same screens):

```sh
A="Authorization: Bearer $OMS_ADMIN"; J="content-type: application/json"

# the trader
PRINCIPAL=$(curl -s -X POST localhost:3001/admin/principals -H "$A" -H "$J" \
  -d '{"code":"alice","display_name":"Alice","principal_type":"HUMAN","status":"ACTIVE"}' | jq -r .id)

# a portfolio
PORTFOLIO=$(curl -s -X POST localhost:3001/admin/portfolios -H "$A" -H "$J" \
  -d '{"code":"main","name":"Main","base_currency":"USD","status":"ACTIVE"}' | jq -r .id)

# an account at Alpaca paper, and make it the portfolio's default
ACCOUNT=$(curl -s -X POST localhost:3001/admin/accounts -H "$A" -H "$J" \
  -d "{\"code\":\"alpaca-paper-main\",\"broker_connection_code\":\"alpaca-paper\",\"external_account_ref\":\"paper\",\"portfolio_id\":\"$PORTFOLIO\",\"status\":\"ACTIVE\"}" | jq -r .id)
curl -s -X PATCH localhost:3001/admin/portfolios/$PORTFOLIO -H "$A" -H "$J" \
  -d "{\"default_account_id\":\"$ACCOUNT\"}" > /dev/null

# a trading token, entitled to trade the portfolio
export OMS_TRADING_TOKEN=$(curl -s -X POST localhost:3001/admin/trading-tokens -H "$A" -H "$J" \
  -d "{\"principal_id\":\"$PRINCIPAL\",\"portfolio_id\":\"$PORTFOLIO\",\"label\":\"laptop\"}" | jq -r .token)
```

`external_account_ref` identifies the account at the broker, and is unique per broker
connection: one OMS account per Alpaca account.

The token is shown once — keep it. It can trade and view `main`, and nothing else: it is
refused on every `/admin` route.

:::note
Skipping the default account makes every order that doesn't name an `account_id` fail
with `400 no account_id given and portfolio has no default account`.
:::

## 5. Send the order

```sh
T="Authorization: Bearer $OMS_TRADING_TOKEN"
ORDER=$(uuidgen | tr A-Z a-z)

curl -s -i -X POST localhost:3001/orders/submit -H "$T" -H "content-type: application/json" -d "{
  \"order_id\": \"$ORDER\", \"client_order_id\": \"first-trade\",
  \"portfolio_id\": \"$PORTFOLIO\", \"symbol\": \"AAPL\",
  \"side\": \"buy\", \"order_type\": \"market\", \"time_in_force\": \"day\", \"quantity\": 1
}"
```

```text
HTTP/1.1 204 No Content
```

`order_id` is yours to generate — it is also the idempotency key, so retrying the same
request can never place the order twice. Read it back:

```sh
curl -s localhost:3001/orders/$ORDER -H "$T"
```

```json
{"order_id":"36276e25-12fe-48fa-81d0-34d2e4240775","client_order_id":"first-trade","portfolio_id":"4e33bc9e-…","account_id":"96cd1db8-…","instrument_id":"14074","side":"buy","order_type":"market","time_in_force":"day","limit_price":null,"original_qty":1.0,"leaves_qty":1.0,"cum_qty":0.0,"avg_px":null,"status":"routed","resume_to_status":null,"version":2}
```

`routed` means Alpaca has it. During US market hours a market order fills within seconds
and the status becomes `filled`, with `avg_px` set. Outside market hours a `day` order
waits at the broker for the open.

You can watch it in the cockpit's blotter too.

## 6. Cancel it (optional)

```sh
curl -s -i -X POST localhost:3001/orders/cancel -H "$T" -H "content-type: application/json" \
  -d "{\"order_id\":\"$ORDER\"}"
```

```text
HTTP/1.1 202 Accepted
```

`202` means the cancel was forwarded to the broker; the order shows `canceled` once
Alpaca confirms — usually within seconds. (A `204` means the OMS cancelled it
outright because it never reached a broker.)

## What you have now

A working install routing to Alpaca paper. Equity prices are not streamed yet — the
OMS has no live equity price feed, so equity positions are not marked to market.

Next: [the trading API in more depth](/openoms/getting-started/first-api-call/), including
the Python client.
