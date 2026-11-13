---
title: Strategies over FIX
description: Connect a strategy or algo to the OMS over FIX 4.4 and trade through its checks.
---

A strategy or algo can send orders to the OMS over FIX. Every order goes through the
same pre-trade checks as an order from the cockpit, is booked by the same order engine,
and shows up in the blotter under the strategy's portfolio. The OMS can stop the strategy
at any time without its cooperation.

Each order adds one hop, about 1–5 ms. A strategy that cannot afford that trades from a
co-located server instead (not covered here).

## Set up a strategy

On the cockpit's **Strategies** page, create a strategy:

| Field | What it is |
|---|---|
| Portfolio | The portfolio the strategy trades. Only the strategy trades it: the cockpit can view it, kill and flatten, but not place orders on it. |
| Responsible user | The active user accountable for the strategy. Required, shown on the Strategies page, and changed only by an audited **Handover**. If that user is disabled the strategy is flagged and cannot be started until you pick another. |
| SenderCompID / TargetCompID | The strategy's session identity. The strategy sends with this SenderCompID to this TargetCompID. |
| Allowed IP addresses | Optional. Comma-separated exact addresses the strategy may connect from; empty means any. |
| Limits | Max order quantity and notional, max position quantity and notional, price band %. |

Then:

1. **Reset password.** The new password is shown once; the OMS keeps only a hash.
2. **Connections.** Pick the strategy's accounts, at most one per broker. An order names
   the broker it goes to (tag 100), which selects that account.
3. **Restart FIX acceptor.** Sessions are created when the acceptor starts. A new
   strategy, or changed CompIDs or IP addresses, need a restart, which logs out every live
   strategy session. A new password, limit or state applies at once.

The acceptor listens only when `[fix_acceptor]` is set in the OMS configuration:

```toml
[fix_acceptor]
port = 9878
certificate = "/etc/oms/fix.pem"
private_key = "/etc/oms/fix-key.pem"
```

## Session

- **FIX 4.4 over TLS**, always. Use the certificate above, or trust its CA.
- **Logon (35=A):** your SenderCompID and TargetCompID, the password in tag 554, and
  `141=Y` (sequence numbers restart at every logon).
- **Logon is refused** for unknown CompIDs, a wrong password, an address outside the allowed
  list, a killed strategy, or a second session while one is live. The reason is shown on
  the Strategies page as the last logon error.
- **After a reconnect** the OMS resends every execution report you missed while you were
  logged out, in order.

## Messages you send

| Message | Tags |
|---|---|
| New Order Single (35=D) | 11 ClOrdID (unique per strategy), 55 Symbol (the broker's symbol), 100 ExDestination (the broker code, e.g. `BINANCE`), 54 Side (1 buy, 2 sell), 38 OrderQty, 40 OrdType (1 market, 2 limit), 44 Price (limit), 59 TimeInForce (0 day, 1 GTC, 3 IOC) |
| Order Cancel Request (35=F) | 11 ClOrdID, 41 OrigClOrdID |
| Order Status Request (35=H) | 11 ClOrdID |
| Security List Request (35=x) | 320 SecurityReqID |

Modifying a live order (35=G) is not supported: cancel it and send a new one. Any message
type not listed here is answered with a Business Message Reject (35=j).

Example, a limit buy of 0.01 BTC on Binance:

```
35=D|11=arb-0001|55=BTCUSDT|100=BINANCE|54=1|38=0.01|40=2|44=61850|59=1
```

## Messages you receive

| Message | When |
|---|---|
| Execution Report (35=8) | For every step of your orders: new (150=0), fill (150=F, with 32 LastQty and 31 LastPx), cancelled (150=4), rejected (150=8), expired (150=C), and for a status request (150=I). 37 is the OMS order id, 17 an exec id unique in the OMS. |
| Order Cancel Reject (35=9) | A cancel that cannot be done, with the reason in 58. |
| Security List (35=y) | Every instrument you may trade: 55 the broker's symbol, 100 the broker code, 48 the OMS instrument id (22=8). |
| Business Message Reject (35=j) | An unsupported message or a malformed one, with the reason in 58. |

## Pre-trade checks

Every check a cockpit order gets, plus the strategy's own:

- **State.** A killed strategy can place nothing; a flattened one only orders that reduce
  a position.
- **Limits** apply per instrument. The strategy's limits are the default; a row on the
  Risk limits page for the strategy's portfolio and one instrument overrides them.
- **Notional** is in the instrument's quote currency. The strategy-wide notional limits
  only apply to instruments quoted in USD, USDT or USDC; an order on any other quote
  currency (for example ETHBTC) needs its own per-instrument limit, or it is rejected.
- **Price band.** A limit price further from the OMS's live mark than the band is rejected.
  A market order's notional is measured at the mark.
- **No mark yet.** If nothing priced the instrument before, the order waits up to 2 seconds
  for a first quote, then is checked; with no quote by then it is rejected
  (`no reference price`). Instruments no feed prices (today: equities) skip the band.

A rejected order comes back as an Execution Report with 150=8, 39=8 and the reason in 58,
for example `risk check failed [max_order_notional]: …`.

## Kill, flatten and disconnects

- **Kill** (cockpit): the strategy is halted, its REST token stops working, every open
  order is cancelled through the OMS's own connections, and its session is logged out.
  Logons are refused until **Enable**. Positions are left as they are.
- **Flatten** (cockpit): everything Kill does, but the strategy may still reduce; then
  every position is closed with a limit order at the edge of the price band
  (good till cancelled). Positions with no live price are skipped and listed.
- **When your session ends** — logout, a dropped connection, missed heartbeats, or an OMS
  restart — the OMS cancels all your open orders, except Flatten's.
