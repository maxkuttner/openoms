---
title: Strategies
description: Run a strategy anywhere, trade the venue yourself, and report its fills to the OMS over HTTP.
---

A strategy trades its venue on its own and sends its fills to the OMS
afterwards. The OMS books them into the strategy's portfolio, so positions and P&L show
in the cockpit. The cockpit labels these **self-managed / unverified**:

- Reports are trusted; the OMS does not check them against the venue.
- The OMS cannot stop the strategy. That is your job.
- If no batch arrives within the strategy's stale threshold, the cockpit warns.

## Manage

On **Trade → Strategies**:

- **Create** makes the strategy, its portfolio and a private group, and shows the
  reporting token **once**.
- **Rotate** issues a new token; the old one works 24 more hours.
- **Revoke** stops all its tokens at once.
- **Archive** revokes its tokens, closes its portfolio and hides it. History is kept;
  strategies are never deleted.

Rotate, revoke and archive need the owner or an admin, with MFA.

## Report fills

```sh
curl -sS -X POST https://oms.example/api/v1/strategies/my-strat/fills \
  -H "Authorization: Bearer <token>" -H "Content-Type: application/json" \
  -d '{
    "schema_version": 1, "batch_id": "b-0001", "sequence": 1,
    "fills": [{
      "venue": "BINANCE", "account_ref": "main", "exec_id": "123456789",
      "symbol": "BTCUSDT", "side": "BUY", "qty": 0.01, "price": 97000.5,
      "fee": 0.97, "fee_asset": "USDT", "trade_time": "2026-11-23T10:14:59Z"
    }]
  }'
```

- `batch_id`: resending one returns the stored result and books nothing.
- `sequence` (optional): increasing; gaps are flagged as `sequence_gap`.
- `venue` + `account_ref` + `exec_id` identify a fill. `symbol` is the venue's.
- `side` `BUY`/`SELL`, `qty` and `price` > 0, `trade_time` RFC 3339.
- `fee` needs `fee_asset`; `client_order_id` is optional.

The response has one outcome per fill: `booked`, `duplicate`, or `break` with a
`break_id`. A bad fill never rejects the batch.

Errors: `400` bad JSON or schema version, `401` bad token, `403` another strategy's
token, `413` over 500 fills, `415` wrong content type, `422` missing field, `429` rate
limit (20 batches/s, bursts of 40).

`GET /api/v1/strategies/{code}/config` with the same token is a handy startup check.

## Breaks

Unbookable fills appear on the breaks page with source `REPORTED`:

- `UNKNOWN_INSTRUMENT`: **adopt** it by picking the instrument, optionally mapping the
  symbol for future fills.
- `INVALID_FILL`, `CONFLICTING_DUPLICATE`: dismiss, then resend a corrected fill with a
  new `exec_id`.
