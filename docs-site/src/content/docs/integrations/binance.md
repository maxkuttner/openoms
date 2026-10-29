---
title: Binance
description: Route crypto spot orders to Binance, over REST or FIX.
---

Binance spot, for crypto. The OMS routes orders to Binance and marks crypto positions
from Binance's public order book.

:::note
This page covers setup only. A full walkthrough, verified end to end on the Binance
testnet, is still to come.
:::

## What it covers

| | |
|---|---|
| Order entry | REST (default) or FIX |
| Executions | The user-data stream (REST) or FIX execution reports |
| Instruments | Spot pairs |
| Catalog | Broker connections → **Instrument universe** in the cockpit |
| Prices | Binance's public spot book; [Bybit](/openoms/integrations/bybit/) as backup |

## Credentials

Binance uses an **Ed25519 API key**: you generate a key pair, register the public key
with Binance, and give the OMS the API key and the **private key PEM** (its contents, not
a path). For the testnet, register at the Binance spot testnet.

Enter them in the cockpit under **Broker connections → binance-paper → Credentials**.
For FIX, also fill in the FIX host, port, `SenderCompID` and `TargetCompID`. Then reload:

```sh
export OMS_ADMIN=$(sed -n 's/^admin_password = "\(.*\)"/\1/p' ~/.config/oms/oms.toml)

curl -s -X POST localhost:3001/admin/connections/reload -H "Authorization: Bearer $OMS_ADMIN"
```

A FIX session cannot be swapped while running: for a FIX connection the reload reports
that a restart is required, and the new credentials apply at the next start.

## Paper or live, REST or FIX

| Variable | Values | Default |
|---|---|---|
| `BINANCE_ENV` | `PAPER` (testnet) or `LIVE` | `PAPER` |
| `BINANCE_PAPER_TRANSPORT` / `BINANCE_LIVE_TRANSPORT` | `rest` or `fix` | `rest` |

## Loading instruments

Saving the credentials offers to load Binance's spot pairs as a background job; the
**Instrument universe** column on **Broker connections** shows its state and has
**Load** / **Reload**. From a terminal: `oms setup sync-broker --broker binance --no-enrich`.
