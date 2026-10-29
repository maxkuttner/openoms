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

A FIX connection needs the host and port of your IBKR FIX gateway, `SenderCompID`,
`TargetCompID` (default `IBKR`), a password, and whether to use TLS (on by default).
IBKR provides these when they enable FIX for your account.

Create a broker connection with broker code `IBKR` in the cockpit, enter the credentials
there, and restart the OMS — a FIX session starts at boot and cannot be swapped while
running.
