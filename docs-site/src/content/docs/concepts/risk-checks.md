---
title: Risk checks
description: What the OMS checks before an order leaves for a broker.
---

Every order passes these checks before it is sent. A failed check rejects the order with
`422` and a message naming the check; nothing reaches the broker.

## Max order value per broker connection

Each broker connection can cap the value of a single order: quantity × price × contract
size, in the instrument's quote currency (USD, USDT, CHF…). It guards against a
fat-finger quantity.

| Environment | Default | Can be removed |
|---|---|---|
| LIVE | 1,000 | No: emptying it is refused; set a different value instead |
| PAPER | none | Yes: empty the field |

Change it on **Broker connections → Edit → Max order value**; the next order uses the
new value.

A limit order is valued at its limit price. A market order is valued at the higher side
of the instrument's current bid/ask. If the OMS has no price for the instrument, a market
order through a capped connection is rejected rather than sent unvalued.

## Limits per portfolio and instrument

Admins can also set limits for one portfolio on one instrument: max order quantity, max
order notional, max position quantity and notional, and a trading state (`ACTIVE`,
`REDUCING`, `HALTED`). With no limits set for that pair, these checks pass. Both kinds of
check run, so the stricter one decides.
