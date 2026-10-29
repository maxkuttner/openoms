---
title: Alpaca
description: Route US equities and options to Alpaca, paper or live.
---

Alpaca is the quickest way to a working setup: free paper trading, US equities and
options, REST order entry and a streaming execution feed. [Your first paper
trade](/openoms/getting-started/first-paper-trade/) walks through it end to end.

## What it covers

| | |
|---|---|
| Order entry | REST |
| Executions | Alpaca's trade-updates stream: fills, partial fills, cancels, rejects |
| Instruments | US equities and ETFs; US equity options for the underlyings you choose |
| Catalog | Broker connections → **Instrument universe** in the cockpit |
| Prices | Not from Alpaca — see [Limits](#limits) |

## Credentials

You need an API key ID and secret from the Alpaca dashboard. Paper and live accounts
have separate keys.

Enter them in the cockpit under **Broker connections → alpaca-paper → Credentials**
(**Test**, then **Save**). They are stored encrypted with your
[master key](/openoms/operations/master-key-and-backups/), never in a file. Apply them to a
running server without a restart:

```sh
export OMS_ADMIN=$(sed -n 's/^admin_password = "\(.*\)"/\1/p' ~/.config/oms/oms.toml)

curl -s -X POST localhost:3001/admin/connections/reload -H "Authorization: Bearer $OMS_ADMIN"
```

`alpaca-paper` should report `Registered`.

### Paper or live

`ALPACA_ENV` chooses which Alpaca connection the OMS uses: `PAPER` (the default) or
`LIVE`. The connection is named after it — `alpaca-paper` or `alpaca-live` — and holds
that environment's keys.

```sh
ALPACA_ENV=LIVE
```

## Loading instruments

Saving the credentials offers to load Alpaca's instrument universe — every tradable US
equity and ETF it lists, about 14,400 — as a background job that takes a few seconds.
After that, the **Instrument universe** column on **Broker connections** shows its state
(○ Not loaded, ◐ Loading, ● Loaded, ◑ Partial, ✕ Failed) with **Load**, **Reload**,
**Load again** or **Retry**. **Jobs** in the top bar has each load's progress and log.

At startup, a connection with credentials whose instruments were never loaded gets a
load automatically; `OMS_SYNC_ON_BOOT=never` turns that off.

From a terminal, the same load is:

```sh
oms setup sync-broker --broker alpaca --no-enrich
```

Option chains are not part of the universe. To seed chains for particular underlyings
from the terminal: `oms setup sync-broker --broker alpaca --underlyings SPY,QQQ --no-enrich`.

FIGI enrichment through OpenFIGI is a separate job,
`POST /admin/broker-connections/alpaca-paper/universe/enrich` — slow without an
`OPENFIGI_API_KEY`.

## Accounts

An OMS account points at a broker connection. For Alpaca, `external_account_ref` can be
any label for the Alpaca account — it must be unique per connection:

```json
{"code":"alpaca-paper-main","broker_connection_code":"alpaca-paper","external_account_ref":"paper","status":"ACTIVE"}
```

## Limits

- **No live equity prices.** The OMS has no equity price feed, so equity and ETF
  positions are not marked to market. Option positions can be marked through
  [Databento OPRA](/openoms/integrations/databento/).
- A handful of Alpaca assets on exchanges the OMS has no venue code for (for example
  `TXSE`) are skipped during sync, with a warning naming them.
- Market orders outside US market hours wait at Alpaca for the open (`day`) — they show
  `routed` until then.
