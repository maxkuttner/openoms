---
title: Your first paper trade
description: Connect Alpaca paper in the cockpit, load its instruments, set up a trader and send 1 share of AAPL.
---

This guide takes a fresh install to a real order at Alpaca's paper-trading venue: one
share of AAPL. Everything until the order itself happens in the cockpit. No market-data
subscription is needed. It assumes the server is running ([First run](/openoms/getting-started/first-run/))
and you are logged in to the cockpit at `http://localhost:3001/cockpit/`.

:::tip[On a remote server?]
The cockpit listens on the server's `localhost`. Forward it to your machine and open
the same address there:

```sh
ssh -L 3001:localhost:3001 yourserver
```
:::

## 1. Get Alpaca paper keys

Create a free account at [alpaca.markets](https://alpaca.markets), switch to **Paper
Trading**, and generate an API key. You get a key ID and a secret.

## 2. Connect Alpaca

Open **Broker connections**, then **Credentials** on the `alpaca-paper` row. Enter the
key ID and secret, click **Test**, then **Save**. The credentials are encrypted with the
master key in `oms.toml`, stored in Postgres, and applied at once — no restart.

## 3. Load Alpaca's instruments

Right after saving, the cockpit asks:

> **Credentials saved ✓**
> Load Alpaca's instrument universe now?
> About 14,400 stocks and ETFs — runs in the background.

Click **Load now**. The load runs as a background job: the top bar shows
**Jobs · 1 running**, and the `alpaca-paper` row's **Instrument universe** column goes
from ◐ Loading to **● Loaded — 14,329 instruments** within a few seconds.

Clicked **Later**? Use the **Load** button in that column whenever you like; **Reload**
refreshes the list later. Click **Jobs** in the top bar to see each load, its progress and
its log.

## 4. Set up a trader

Orders are placed by a **principal** (a person, strategy or service) into a
**portfolio**, which books through an **account** at a broker. Create one of each:

1. **Principals → New** — Code `alice`, Display name `Alice`, Type `HUMAN`, Status `ACTIVE`.
2. **Portfolios → New** — Code `main`, Name `Main`, Status `ACTIVE`.
3. **Accounts → New** — Code `alpaca-paper-main`, Broker connection `alpaca-paper`,
   External account ref `paper`, Portfolio `main`, Status `ACTIVE`.
4. **Portfolios → Edit** `main` — set **Default account (routing)** to `alpaca-paper-main`.

:::note
Skip step 4 and every order that doesn't name an `account_id` is refused with
`400 no account_id given and portfolio has no default account`.
:::

## 5. Get a trading token

Open **API**, pick Principal `alice` and Grant portfolio `main`, give it a label such as
`laptop`, and click **Create token**. The token is shown **once** — copy it now:

```sh
export OMS_TRADING_TOKEN=paste-the-token-here
```

It can trade and view `main`, and nothing else — it is refused on every `/admin` route.

## 6. Send the order

```sh
PORTFOLIO=$(curl -s localhost:3001/portfolios -H "Authorization: Bearer $OMS_TRADING_TOKEN" | jq -r '.[0].portfolio_id')
ORDER=$(uuidgen | tr A-Z a-z)

curl -s -i -X POST localhost:3001/orders/submit \
  -H "Authorization: Bearer $OMS_TRADING_TOKEN" -H "content-type: application/json" -d "{
  \"order_id\": \"$ORDER\", \"client_order_id\": \"first-trade\",
  \"portfolio_id\": \"$PORTFOLIO\", \"symbol\": \"AAPL\",
  \"side\": \"buy\", \"order_type\": \"market\", \"time_in_force\": \"day\", \"quantity\": 1
}"
```

```text
HTTP/1.1 204 No Content
```

`order_id` is yours to generate — it is also the idempotency key, so retrying the same
request can never place the order twice. Watch it in the cockpit's **Blotter**, or read it
back:

```sh
curl -s localhost:3001/orders/$ORDER -H "Authorization: Bearer $OMS_TRADING_TOKEN"
```

`routed` means Alpaca has it. During US market hours a market order fills within seconds
and the status becomes `filled`, with `avg_px` set. Outside market hours a `day` order
waits at the broker for the open.

To cancel it while it is still working:

```sh
curl -s -i -X POST localhost:3001/orders/cancel -H "Authorization: Bearer $OMS_TRADING_TOKEN" \
  -H "content-type: application/json" -d "{\"order_id\":\"$ORDER\"}"
```

`202` means the cancel was forwarded to the broker; the order shows `canceled` once Alpaca
confirms — usually within seconds.

## What you have now

A working install routing to Alpaca paper. Equity prices are not streamed yet — the
OMS has no live equity price feed, so equity positions are not marked to market.

Next: [the trading API in more depth](/openoms/getting-started/first-api-call/), including
the Python client.

## Prefer the API?

Everything in steps 2–5 is also an admin API call, authenticated with the cockpit
password:

```sh
export OMS_ADMIN=$(sed -n 's/^admin_password = "\(.*\)"/\1/p' ~/.config/oms/oms.toml)
A="Authorization: Bearer $OMS_ADMIN"; J="content-type: application/json"; B=localhost:3001

# 3. load Alpaca's instruments (202 + a job id; follow it at /admin/jobs/{id})
curl -s -X POST $B/admin/broker-connections/alpaca-paper/universe/load -H "$A"
curl -s $B/admin/broker-connections/alpaca-paper/universe -H "$A"   # state: loading → loaded

# 4. the trader, portfolio and account
PRINCIPAL=$(curl -s -X POST $B/admin/principals -H "$A" -H "$J" \
  -d '{"code":"alice","display_name":"Alice","principal_type":"HUMAN","status":"ACTIVE"}' | jq -r .id)
PORTFOLIO=$(curl -s -X POST $B/admin/portfolios -H "$A" -H "$J" \
  -d '{"code":"main","name":"Main","base_currency":"USD","status":"ACTIVE"}' | jq -r .id)
ACCOUNT=$(curl -s -X POST $B/admin/accounts -H "$A" -H "$J" \
  -d "{\"code\":\"alpaca-paper-main\",\"broker_connection_code\":\"alpaca-paper\",\"external_account_ref\":\"paper\",\"portfolio_id\":\"$PORTFOLIO\",\"status\":\"ACTIVE\"}" | jq -r .id)
curl -s -X PATCH $B/admin/portfolios/$PORTFOLIO -H "$A" -H "$J" -d "{\"default_account_id\":\"$ACCOUNT\"}" > /dev/null

# 5. a trading token
export OMS_TRADING_TOKEN=$(curl -s -X POST $B/admin/trading-tokens -H "$A" -H "$J" \
  -d "{\"principal_id\":\"$PRINCIPAL\",\"portfolio_id\":\"$PORTFOLIO\",\"label\":\"laptop\"}" | jq -r .token)
```

Credentials themselves are entered in the cockpit (or imported once with
`oms config import-env`). Every endpoint is in the [API reference](/openoms/api/).
