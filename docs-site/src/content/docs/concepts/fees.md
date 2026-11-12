---
title: Fees
description: How OpenOMS books the fees a venue charges on fills, and what they do to positions and P&L.
---

OpenOMS books the fee a venue reports for a fill. Nothing is estimated: if the
venue sends no fee, none is booked. Each fee is stored as its own order event
(`order_fee_charged`) with an amount and a currency, so it shows in the order's
event list and the audit trail.

A charged fee is a positive amount. A rebate is negative, whatever sign the venue
uses.

## Three effects

What a fee does depends on its currency compared with the instrument.

**Quote currency.** The fee reduces realized P&L. Trade BTCUSDT and pay a 0.50 USDT
fee, and realized P&L is 0.50 lower. The P&L figure from the API is already net of
these fees.

**Base asset.** The fee reduces the position's quantity at average cost. Buy 1 BTC
on BTCUSDT and pay a 0.001 BTC fee, and the position is 0.999 BTC, which is what the
venue holds. The order's filled quantity is not changed: the fill is what traded.

**Any other currency.** The fee is recorded and shown as a per-currency total in
**Other fees** on the position row. It does not touch quantity or P&L. A 0.01 BNB fee
on BTCUSDT, or a USD commission on a CAD-quoted stock, lands here.

Fees are never converted between currencies. Totals stay per currency.

## Rebates

A maker rebate books as a negative fee. In the quote currency it increases realized
P&L; in another currency it lowers that currency's total.

## Running-total venues

OKX and Coinbase report a running fee total per order, not a fee per fill. OpenOMS
books the difference between the new total and what is already booked for that
order. A lower total books a negative correction.

## Venue notes

- **Binance:** the fee and its currency come with each trade.
- **Bybit:** the fee currency is the one Bybit reports (`feeCurrency`). When it
  reports none, a buy is charged in the base coin and a sell in the quote coin.
- **OKX, Coinbase:** running total per order, as above.
- **Interactive Brokers (TWS):** the commission arrives just after the fill, so it
  appears as a separate fee entry slightly later than the fill itself.
- **Alpaca:** no commission is reported, so nothing is booked.

## Limits

- **No backfill.** Only fills booked after fee booking was introduced carry fees.
  Existing orders and positions are not revisited.
- **No FX.** There is no conversion, so a position's fees in several currencies are
  shown side by side.
- **Reconnect reconciliation carries no fee.** When the OMS learns of a fill only
  through reconnect reconciliation, which reports order state rather than the trade
  stream, the fill is booked without a fee. This holds for every venue. Fees on fills
  that happen while a stream is down can therefore be missed.

## Where fees show

In the trade app, the orders blotter has a **Fees** column (per currency, joined
with ` · `, empty when none) and the positions table has an **Other fees** column.
The API returns `fees` on each order and `other_fees` on each position.
