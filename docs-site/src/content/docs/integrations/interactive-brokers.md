---
title: Interactive Brokers (FIX)
description: Order entry to Interactive Brokers over a FIX session.
---

The OMS can run a FIX order-entry session to Interactive Brokers, using the QuickFIX
engine built into the binary. QuickFIX handles logon, heartbeats, sequence numbers,
resends, TLS and reconnects.

:::caution[Limited]
There is no catalog sync for IBKR: `oms setup sync-broker` does not support it, so no
command creates the IBKR mappings an order needs to route. IBKR order entry works only
for instruments that already have one. Treat this integration as experimental.
:::

## Credentials

IBKR gives you the FIX session details when they enable FIX for your account. Create a
broker connection in the cockpit with **Interactive Brokers** as the venue; its
credentials drawer asks for:

| Field | What it is | Default |
|---|---|---|
| Password | Your FIX session password | |

and, under **FIX session**:

| Field | What it is | Default |
|---|---|---|
| Host | Your IBKR FIX gateway's host | |
| Port | Its port | |
| SenderCompID | Your side of the session, assigned by IBKR | |
| TargetCompID | IBKR's side of the session | `IBKR` |
| TLS | Encrypt the session; leave on unless IBKR says otherwise | on |

Then restart the OMS: a FIX session starts at boot and cannot be swapped while running.
