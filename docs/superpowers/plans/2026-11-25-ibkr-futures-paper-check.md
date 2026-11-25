# IBKR futures: manual paper check

Run against an IBKR paper account with MES. Fill in each result line (pass/fail plus notes).

## 1. Sync and contract size
Do: `POST /admin/futures/sync`, then look up MES contracts.
Pass: MESZ6 and MESH7 exist with `contract_size 5`.
Result:

## 2. Buy the front month
Do: type `MES` in instrument search, buy 1 of the front contract, let the price move.
Pass: the order fills; position P&L moves by 5 x the price change.
Result:

## 3. Roll warning
Do: temporarily set `roll_warn_days` high for MES while holding the position.
Pass: cockpit badge and banner appear, and a `ROLL_DUE` record reaches the Kafka `alerts` topic.
Result:

## 4. Roll
Do: press Roll and submit.
Pass: one BAG order at IBKR, child orders filled, positions MESZ6 0 and MESH7 1.
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
Do: submit a roll BAG with directional legs and a negative limit price.
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
Pass: all render and behave as described.
Result:
