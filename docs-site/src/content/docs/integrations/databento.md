---
title: Databento / OPRA
description: Live US options prices from Databento's OPRA feed, to mark option positions.
---

Databento's OPRA feed gives the OMS live prices for US equity options, so option
positions are marked to market and show unrealized P&L. It is a **price source only**.

## What it covers

- The consolidated top of book (NBBO) across the US options exchanges — Databento's
  `cmbp-1` schema.
- Only the contracts you hold: the OMS subscribes to held option positions and adds
  newly opened ones mid-session.

You need a Databento account with live OPRA access, which is a paid subscription.

## Credentials

One API key. Enter it in the cockpit under **Data feeds → databento-opra →
Credentials** (**Test**, then **Save**), then reload:

```sh
export OMS_ADMIN=$(sed -n 's/^admin_password = "\(.*\)"/\1/p' ~/.config/oms/oms.toml)

curl -s -X POST localhost:3001/admin/connections/reload -H "Authorization: Bearer $OMS_ADMIN"
```

`databento-opra` should report `Registered`.
