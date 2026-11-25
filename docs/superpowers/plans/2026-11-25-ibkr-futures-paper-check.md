# IBKR futures: manual paper check

Run against an IBKR paper account with MES. Fill in each result line (pass/fail plus notes).
"Current" and "next" below mean the MES front contract and the one after it on the day
you run this (look them up with `GET /futures/roots`).

## 1. Sync and contract size
Do: `POST /admin/futures/sync`, then look up MES contracts.
Pass: the current and next MES contracts exist with `contract_size 5`.
Result:

## 2. Buy the front month
Do: type `MES` in instrument search, buy 1 of the front contract, let the price move.
Pass: the order fills; position P&L moves by 5 x the price change.
Result:

## 3. Roll warning
Do: temporarily set `roll_warn_days` high for MES while holding the position
(`UPDATE future_root SET roll_warn_days = 60 WHERE code = 'MES'`), then run the expiry
sweep, which raises roll warnings (`POST /admin/instruments/expiry-sweep`, admin; it also
runs hourly).
Pass: cockpit badge and banner appear, and a `ROLL_DUE` record reaches the Kafka
`oms.alerts` topic (key = portfolio id, value = the alert JSON).
Then: reset it (`UPDATE future_root SET roll_warn_days = 3 WHERE code = 'MES'`).
Result:

## 4. Roll
Do: press Roll and submit.
Pass: one BAG order at IBKR, child orders filled, positions in the current contract 0 and
in the next contract 1.
Result:

## 5. Restart mid-roll
Do: start a roll with a working LMT far from the market, restart the OMS, then cancel.
Pass: parent still working after restart, children untouched, cancel works.
Result:

## 6. Local-symbol-only contract lookup
Do: restart the OMS with a cold cache and let futures prev close load.
Pass: IBKR accepts a lookup by local symbol only, with an empty exchange; prev close appears.
Result:

## 7. BAG with directional legs and negative price
Do: submit a roll BAG with directional legs and a negative limit price. A negative net
debit comes from rolling a short (buy the current contract back, sell the next) when the
next contract trades above the current one, which is usual for index futures: hold a
short of 1 in the current contract, open the roll dialog, and the suggested price is
already negative. Or, for a long, type a price below zero by hand (far from the market,
so it rests).
Pass: IBKR accepts the order.
Result:

## 8. Leg executions carry the parent's orderRef
Do: fill a roll, check executions in the live stream, then restart and check `reqExecutions`.
Pass: leg executions carry the parent's orderRef in both.
Result:

## 9. Futures last-trade time
Do: inspect a synced contract's `expires_at` against IBKR's contract details.
Pass: note whether IBKR sends a last trade time (lastTradeTime or in the date string) or `expires_at` falls back to the venue calendar.
Result:

## 10. Cockpit in a browser
Do: search "MES"; check the badge and banner; open the roll dialog (try a negative price and a quantity); check the roll legs pill in the blotter.
Pass: all render and behave as described; the quantity takes whole contracts only; no
calendar-spread instrument (`MES…-MES…`) shows up in instrument search.
Result:

## 11. CME calendar spread tick
Do: in TWS, open the MES (and if possible ES and NQ) calendar spread of the current and
next contracts and read its price increment.
Pass: it is 0.05 index points, matching `future_root.spread_tick`. If not, update the row
(`UPDATE future_root SET spread_tick = <tick> WHERE code = '<root>'`) and record it here.
Result:

## 12. Default spread price on a one-tick book
Do: with each leg quoted one tick wide (mid on a half tick, e.g. x.125), open the roll
dialog and submit the suggested limit price unchanged.
Pass: the suggested price is a multiple of the spread tick (0.05) and IBKR accepts it;
the OMS does not answer 422.
Result:

## 13. Dismissed alert on an overdue contract
Do: hold the current contract past its roll date (or set `roll_offset_days` so the roll
date is yesterday), run the expiry sweep so the alert is raised, dismiss its banner alert, then look at the
position.
Pass: the position still shows the Roll action (badge "Roll overdue"), and the roll
dialog opens on the next contract.
Then: reset `roll_offset_days` to 8 if you changed it.
Result:

## 14. Futures prev close without CME data
Do: on an account with no CME market data subscription, hold a stock and a future, and
open Positions.
Pass: the stock still has a previous close and its Day P&L; only the future is listed
as missing a previous close. The OMS log shows one warning for the future's previous
close, not a failed batch.
Result:
