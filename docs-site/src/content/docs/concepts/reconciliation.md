---
title: External orders and reconciliation
description: How OpenOMS picks up orders and fills it did not place, books them into a book, and raises breaks when it cannot.
---

Orders do not only come from the OMS. You may trade from the venue's own app,
another API client, or a script. OpenOMS watches each connection for these
orders and fills so positions stay right.

## What happens to an order the OMS did not place

1. **Attribution rules.** A rule matches on the venue account, the
   client-order-id prefix, or both, and names a book. Rules have a priority;
   the lower number wins.
2. **Connection default book.** If no rule matches, the order goes to the
   connection's default book, if one is set.
3. **Break.** If neither applies, nothing is booked. The order is raised as a
   break for you to resolve.

An order that is adopted behaves like any other order in its book: fills, fees
and status changes are booked. You can cancel it from the OMS. You cannot amend
it.

## Three layers

Each connection is kept in sync by three layers, so a missed message is caught
later.

- **Stream.** Real-time updates from the venue.
- **Intraday pull.** Fetches fills since a saved position. A new account starts
  at "now", so history is not back-filled unexpectedly.
- **End-of-day sweep.** Runs daily over a lookback you choose: 1 day, 1 week,
  1 month, 1 year, 5 years or 10 years. The window is clamped to what the venue
  keeps, and the report shows the window actually obtained.

Fills are de-duplicated, so overlapping windows never book a fill twice.

### What each venue keeps

| Venue | History available |
| --- | --- |
| IBKR (TWS) | About 7 days of executions |
| IBKR (FIX) | No fill history; open-order snapshot only |
| OKX | 90 days |
| Bybit | 2 years |
| Alpaca | No cap |
| Coinbase | No cap |
| Binance | Per symbol. Pairs are discovered from open orders, orders booked in the OMS, and non-zero balances |

## Observed orders

An order the OMS did not place starts as **Observed at venue**. It has no
submitted or routed step, because the OMS never sent it. Observing an order
books no fills; the fills follow, and the order is not closed until the booked
quantity reaches the order quantity. An observed order that the venue reports as
Filled can therefore briefly show less booked than filled.

## Order times

Every order shows four times.

- **Placed.** For an order the OMS placed, when it was submitted. For an external
  order, the venue's create time where the venue gives one, otherwise the time
  the OMS first saw it.
- **Trade time.** The venue's execution time of the last fill. A `~` in front
  means it is estimated: the venue sent no execution time, so the booking time
  is used.
- **Booked.** When the OMS recorded the order or fill.
- **Updated.** The last change to the order.

Each fill and history row shows both its venue time and its booking time.

Venues that report a create time: Binance, Alpaca, Bybit, OKX and Coinbase. IBKR
(TWS and FIX) do not, so Placed shows when the order was first seen.

Orders from before trade times were added keep their old history, and their
trade time is the booking time. Legacy external orders keep their old
submitted and routed history.

## Positions follow trade time

Positions and realized P&L are ordered by trade time, not by when a fill was
booked. A fill that arrives late is placed where it traded, and the position is
recalculated from that point. Fills with the same trade time are ordered by fill
id, and a fee always follows its fill.

## Breaks

A break is something the OMS saw at the venue but could not book on its own,
for example an order with no matching rule and no default book, or an unknown
instrument. Open the breaks page and either:

- **Adopt** it into a book. The order and its fills are booked there.
- **Dismiss** it. Nothing is booked.

An unknown-instrument break can only be adopted once the instrument resolves;
until then adopting returns an error and the break stays open.

If a break grows (more fills arrive), adopting re-reads it first.

## Multiple accounts

Only IBKR supports several accounts: one login can cover several accounts, and
the accounts list is shown only for IBKR connections. Orders are tagged with the
IBKR account they were placed on, which is what attribution rules match. The account is
chosen when an admin grants the portfolio the IBKR connection.

Binance sub-accounts are not listed under one connection. A sub-account is its
own API key; today there is one Binance connection per environment.

## IBKR setup

Set **Master API client ID** in the IB Gateway or TWS API settings to the
connection's `master_client_id` (default 0). Without it, TWS does not report
orders placed by other API clients, and they are never seen. Client id 0 also
sees orders entered manually in TWS.

Upgrading: IBKR orders still working under the old client id 1 become invisible.
Cancel them and place them again.

## Known gaps

- On Binance, a pair the OMS has never seen, bought and sold back to zero
  entirely while the stream was down, leaves no open order, booked order or
  balance, so no pull finds it.
- A venue correction to an already-booked fill is logged as a warning. It is not
  amended.
- A trade bust (FIX) is logged as a warning. It is not reversed in the books.
