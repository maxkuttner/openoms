---
title: Reported strategies
description: Run a strategy anywhere, trading the venue yourself, and report its fills to the OMS over HTTP.
---

A **reported** strategy trades its venue on its own: its own server, its own venue
account, its own risk checks. It sends its fills to the OMS after the fact, and the OMS
books them into the strategy's portfolio, so fills, positions and P&L show up in the
cockpit alongside everything else.

The cockpit labels these strategies **self-managed / unverified**.

## Trust model

- **Reports are trusted.** The OMS does not check fills against the venue. What you
  report is what gets booked.
- **No kill switch.** The OMS has no connection to your venue account, so it cannot stop
  the strategy. Stopping it is your job.
- **"Reports stopped" is the only liveness signal.** If no batch arrives for a while,
  the cockpit shows a stale warning. That is all the OMS knows about whether the
  strategy is running.

If you need the OMS to watch the venue account and be able to stop the strategy, use a
strategy that trades [through the OMS](/strategies/fix/) instead.

## Create one

1. In the cockpit, open **Strategies** and create a reported strategy. A portfolio and a
   private group are created with it. You own it; admins can see and act on it too.
2. Mint its reporting token. **The token is shown once.** Copy it then.
3. **Rotate** mints a new token; the old one keeps working for 24 hours so you can
   redeploy without dropping reports. **Revoke** stops a token at once (requests get
   `401`).

## Reporting fills

```
POST /api/v1/strategies/{code}/fills
Authorization: Bearer <token>
Content-Type: application/json
```

```json
{
  "schema_version": 1,
  "batch_id": "2026-11-23T10:15:00Z-0001",
  "sequence": 1,
  "fills": [
    {
      "venue": "BINANCE",
      "account_ref": "main",
      "exec_id": "123456789",
      "client_order_id": "my-order-42",
      "symbol": "BTCUSDT",
      "side": "BUY",
      "qty": 0.01,
      "price": 97000.5,
      "fee": 0.00001,
      "fee_asset": "BTC",
      "trade_time": "2026-11-23T10:14:59.123Z"
    }
  ]
}
```

| Field | Required | Notes |
| --- | --- | --- |
| `schema_version` | yes | Must be `1`; anything else is `400`. |
| `batch_id` | yes | Your id for the batch. Resending a `batch_id` returns the stored result and books nothing. |
| `sequence` | no | Increasing batch counter. Gaps are flagged (`sequence_gap`) and shown in the cockpit. |
| `venue`, `account_ref`, `exec_id`, `symbol` | yes | Non-empty. `venue` + `account_ref` + `exec_id` identifies a fill. `symbol` is the venue's symbol. |
| `client_order_id` | no | |
| `side` | yes | `BUY` or `SELL` (any case). |
| `qty`, `price` | yes | Greater than 0. |
| `fee`, `fee_asset` | no | `fee` ≥ 0, and needs `fee_asset`. A fee in the quote or base asset adjusts the position; any other asset is recorded only. |
| `trade_time` | yes | RFC 3339 timestamp. |

Other fields are ignored. Fills always book into the strategy's own portfolio.

### Response

`200` with one outcome per fill, in order:

```json
{
  "batch_id": "2026-11-23T10:15:00Z-0001",
  "results": [
    { "exec_id": "123456789", "outcome": "booked", "break_id": null }
  ],
  "sequence_gap": false
}
```

- `booked`: the fill is in the portfolio.
- `duplicate`: the same fill was already booked; nothing changes.
- `break`: the fill could not be booked and became a recon break (`break_id`). Reasons:
  `UNKNOWN_INSTRUMENT` (symbol not mapped for that venue), `INVALID_FILL` (failed the
  checks above), `CONFLICTING_DUPLICATE` (same exec id, different contents).

A bad fill never rejects the batch: the rest still books.

Errors: `400` bad body or schema version, `401` missing/invalid/revoked token, `403` token
belongs to another strategy, `404` unknown strategy, `413` more than 500 fills, `429`
rate limit.

### Limits

- 500 fills per batch.
- 20 batches per second per strategy.

### Config

`GET /api/v1/strategies/{code}/config` with the same token returns the strategy code, its
`portfolio_code`, `schema_version`, `max_batch_fills` and `max_batches_per_sec`. Useful as
a startup check.

## Breaks and adoption

Breaks from reported fills appear on the cockpit's breaks page with source `REPORTED`.
For each one you can:

- **Adopt** it: pick the instrument (and, if needed, a different portfolio). The stored
  fill is booked exactly once. Add the venue symbol mapping as well and later fills for
  that symbol book normally.
- **Dismiss** it: the fill is dropped.

## Example

```sh
curl -sS -X POST https://oms.example/api/v1/strategies/my-strat/fills \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "schema_version": 1,
    "batch_id": "b-0001",
    "sequence": 1,
    "fills": [{
      "venue": "BINANCE", "account_ref": "main", "exec_id": "123456789",
      "symbol": "BTCUSDT", "side": "BUY", "qty": 0.01, "price": 97000.5,
      "fee": 0.97, "fee_asset": "USDT", "trade_time": "2026-11-23T10:14:59Z"
    }]
  }'
```

Send the same command again and you get the same response back (same `batch_id`).
Change the `batch_id` and the fill comes back as `duplicate`.

## SDK

A client SDK that handles batching, retries and sequencing is coming. Until then, the
HTTP API above is all you need.
