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
# the host token sits next to oms.toml; the server accepts it from this machine only
curl -s -X POST localhost:3001/admin/connections/reload \
  -H "Authorization: Bearer $(cat ~/.config/oms/host-token)"
```

`databento-opra` should report `Registered`.
