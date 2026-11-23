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
- **Reports stopping is the only liveness signal.** If no batch arrives for longer than
  the strategy's stale threshold (configurable per strategy), the cockpit shows a stale
  warning. A strategy that has never reported shows as stale too. That is all the OMS
  knows about whether the strategy is running.

If you need the OMS to watch the venue account and be able to stop the strategy, use a
strategy that trades [through the OMS](/strategies/fix/) instead.

## Create one

1. In the cockpit, open **Trade → Strategies** and create a reported strategy. A portfolio and a
   private group are created with it. You own it; admins can see and act on it too.
   Rotating and revoking require the owner or an admin, with MFA.
2. Creating it issues the reporting token. **The token is shown once.** Copy it then.
3. **Rotate** mints a new token; the old one keeps working for 24 hours so you can
   redeploy without dropping reports. Only one old token is kept in its grace period:
   rotating again revokes it at once. **Revoke** stops all the strategy's tokens at once
   (requests get `401`).

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

Errors:

- `400`: the body is not valid JSON, or `schema_version` is not `1`.
- `415`: `Content-Type` is missing or not `application/json`.
- `422`: a top-level field is missing or has the wrong type (for example no `batch_id`,
  or `fills` is not an array).
- `401`: missing, invalid, expired or revoked token (or the strategy is inactive).
- `403`: the token belongs to another strategy.
- `413`: more than 500 fills.
- `429`: rate limit.

### Limits

- 500 fills per batch.
- 20 batches per second per strategy sustained, with bursts up to 40.

### Config

`GET /api/v1/strategies/{code}/config` with the same token returns the strategy code, its
`portfolio_code`, `schema_version`, `max_batch_fills` and `max_batches_per_sec`. Useful as
a startup check.

## Breaks and adoption

Breaks from reported fills appear on the cockpit's breaks page with source `REPORTED`.
What you can do depends on the kind:

- `UNKNOWN_INSTRUMENT` can be **adopted**: pick the instrument (and, if needed, a
  different portfolio). The stored fill is booked exactly once. Choose to map the venue
  symbol to that instrument as well, and later fills for that symbol book normally.
- `INVALID_FILL` and `CONFLICTING_DUPLICATE` are **dismiss-only**: adopting them is
  refused, since the OMS cannot invent missing fields or overrule a booked fill. Dismiss
  the break and, if the fill was real, resend a corrected fill with a new `exec_id`.
- **Dismissing** any break drops the fill.

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
Change the `batch_id` (and set `sequence` to `2`) and the fill comes back as
`duplicate`. Keep `sequence: 1` and the response also flags `sequence_gap: true`.

## SDK

A client SDK that handles batching, retries and sequencing is coming. Until then, the
HTTP API above is all you need.
