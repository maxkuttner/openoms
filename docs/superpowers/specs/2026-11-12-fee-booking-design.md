# Fee booking — design

Date: 2026-11-12. Status: draft for approval.

## Why

Fills carry no fee today, so realized P&L in the cockpit is overstated on every
broker. Venues already send the fees: Binance's are parsed and dropped, IBKR's
commission reports are only logged, colo runtime fills store a fee that is never
booked. The user wants realized P&L net of fees and a fee column on fills.

## Goals

- Every new fill books the fee the venue reports for it: amount and currency.
- Realized P&L is net of fees paid in the position's P&L (quote) currency.
- A fee paid in the instrument's base asset reduces the position's quantity
  (buy 1 BTC, pay 0.001 BTC fee → hold 0.999), matching what the venue holds.
- A fee in any other currency (BNB on Binance; USD commission on a CAD stock)
  is recorded and shown as a per-currency fee total, never converted.
- The cockpit shows a fee column (amount + currency) on fills, and realized
  P&L that already includes fees.

## Non-goals

- No backfill: fills booked before this ships stay fee-less (user decision).
- No FX conversion of fees, ever (consistent with the per-currency rule from
  the IBKR work).
- No fee estimates or fee schedules: only fees the venue reports.
- No BNB (or other third-asset) holdings: the OMS keeps positions per
  instrument, not per asset; a third-asset fee is a total, not a position.
- No fee-aware pre-trade risk (colo's fee allowance stays as is).

## Anti-goals

- A fee booked twice (replayed session, snapshot recon, reconnect, restart
  catch-up). Fees must be idempotent per venue fee identity.
- A fee that silently disappears because it arrived before or after its fill.
- Mutating or deleting `order_event` rows (append-only).

## Decided by the user

- Fees per currency, no FX.
- Base-asset fees reduce the held quantity.
- Only new fills; no backfill.

## Design (calls made in brainstorming, the builder may refine)

1. **Fees are their own order event.** A new event type (working name
   `order_fee_charged`) with payload `{ execution_id: Option<String>, amount,
   currency, fee_id }`, appended through the order engine like any execution
   event. Separate from the fill because IBKR's commission report arrives after
   the execution, Coinbase/OKX report a running fee total per order rather than
   per fill, and `order_event` is append-only. When a venue gives the fee with
   the fill (Binance, Bybit), the engine appends fill and fee together.
   `fee_id` is the dedupe key (venue exec/trade id, or for cumulative venues the
   order id plus the cumulative total reached).
2. **Contract.** The gateway's `ExecKind` gets a fee variant (or `Fill` gets an
   optional fee) so connectors pass fees through the existing engine channel.
   For cumulative venues the connector or engine books the delta since the last
   booked total, mirroring how `Cumulative` qty deltas become fills today.
3. **Position math.** `PositionMath` gains `apply_fee(amount, currency,
   base, quote)`: quote currency → `realized_pnl -= amount`; base asset →
   reduce `net_qty` at cost (like `reduce_at_cost`); otherwise no position
   change. `rebuild_positions` replays fee events so a rebuild gives the same
   answer. Which currency is base/quote comes from the instrument (crypto pairs:
   base/quote assets; equities/options: the instrument currency is the quote,
   no base asset).
4. **Third-currency totals.** Stored per (portfolio, currency) as a small read
   model (or derived from events on read — builder's call), shown in the
   cockpit next to realized P&L.
5. **Connectors** (all new fills only):
   - Binance: per-trade `n`/`N` already parsed → pass through.
   - Bybit: `execFee` on executions (+ fee currency per Bybit's rules for spot).
   - OKX: `fillFee`/`fillFeeCcy` on order updates (OKX reports fees negative:
     normalise sign so a charged fee is positive, a rebate negative).
   - Coinbase: `total_fees` running total per order → delta, quote currency.
   - IBKR_TWS: `Commission` report, matched to its execution by exec id;
     replace the log line. Arrives after the fill: append as its own event.
   - Alpaca: no commission; nothing to do.
   - Colo runtime / followed external fills: the ledger already stores fee;
     book it when the fill is booked.
6. **Rebates.** A negative fee (maker rebate) books as a negative fee and
   increases realized P&L. Not filtered out.
7. **Surfaces.** Fee column on fills in the cockpit (amount + currency, blank
   when none), order detail event list shows fee events, positions show
   realized P&L net of quote-currency fees plus the third-currency fee totals.
   API: fills/events expose the fee; OpenAPI regenerated. Kafka fill/position
   records carry the fee if the outbox mapping makes that cheap (builder's
   call; note it if skipped).
8. **Migrations.** Only additive (new event type, any read-model table or
   column). Existing positions untouched.

## Calls settled after the builder check (my calls, user to confirm)

- **Order does not matter.** A fee event names its order. The position effect
  is applied when the fee is booked, whether or not its fill has been booked
  yet. No pending buffer. Rebuild replays events in order and gets the same
  result, because quote and base fee effects don't depend on the fill.
- **Where base/quote come from.** The instrument comes from the order. The
  engine resolves the fee's effect at booking time (`quote` | `base` | `other`)
  and stores it in the event payload, so a rebuild needs no lookups:
  - crypto pair: base and quote assets;
  - equities, options, futures: quote = instrument currency, no base;
  - IBKR forex pairs: same as crypto (base and quote currencies).
- **Base-asset fee.** Always reduces the position's quantity by the fee at
  average cost (`net_qty -= amount` for a long, more negative for a short):
  - applies after a close or after later fills too (the venue really took it);
  - the order's `filled_qty` is never touched; the fill is what traded.
- **Cumulative venues.** The last booked total per order is the sum of that
  order's fee events (read from events, so it survives restarts).
  - A new report books `total − booked`.
  - A lower total books a negative correction.
  - `fee_id` = venue order id + the total reached.
  - Per-fill venues use the venue's trade or exec id.
  - Duplicates are ignored at append (unique per order + `fee_id`).
- **Colo / followed fills.** One path only: the ledger's fee (amount and
  currency, already stored) is booked with the external fill through the same
  fee event. A followed account's fills don't also arrive through a venue
  connector. "New fills only" means only fills booked after the release; rows
  already booked are never revisited.
- **Compatibility.** New event type and new fields are additive. Single-user
  prerelease, no external consumers to protect. Kafka fill records gain
  optional `fee_amount` / `fee_currency`.
- **Third-currency totals** per (portfolio, instrument, currency), all time:
  - shown on the position row;
  - per-portfolio totals are sums of those rows.
- **Bybit and OKX fee-currency rules:** the builder looks them up in the venue
  docs and records them in the connector docs; no testnet run is needed to
  plan.

## Constraints

- `order_event` is append-only; DB tests seed their own data and never delete
  from it; CI runs `--ignored` DB tests on a fresh Postgres.
- Numbers through the existing f64/NUMERIC conventions; send-without-float-noise
  rules unchanged.
- No credentials in code, tests or commits.

## Left to the builder

Exact event/type names, whether the fee rides inside `ExecKind::Fill` or as a
separate `ExecKind` variant, the read model for third-currency totals, and the
blotter's exact layout (no help text in the UI; docs explain it).

## Done means

- Unit tests per connector: fee parsed and passed through; cumulative deltas;
  IBKR late commission; replay/duplicate does not double-book.
- Position math tests: quote fee, base fee, third-currency fee, rebate,
  rebuild equals incremental.
- DB tests for the event round-trip and rebuild.
- A Binance testnet fill (testnet fees are 0, so this proves plumbing only) and
  an IBKR paper fill show the fee in the cockpit; docs page on fees.
