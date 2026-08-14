# oms-client

Python client for the OMS trading API. Takes a trading token and nothing else — it
never touches the admin surface.

```bash
pip install -e clients/python            # add [pandas] for .to_pandas()
```

## Getting a token

Trading tokens are minted by an admin, once, and shown once:

```bash
curl -X POST "$OMS_URL/admin/trading-tokens" \
  -H "Authorization: Bearer $OMS_ADMIN_PASSWORD" \
  -H 'Content-Type: application/json' \
  -d '{"principal_id":"<uuid>","portfolio_id":"<uuid>","label":"my-laptop"}'
```

The `token` field of the response is what this client wants. It carries the
principal's grants (`can_trade` / `can_view` / `can_allocate`) on the portfolios it
was granted, and can do nothing else.

## Library

```python
from oms_client import OMS

oms = OMS("http://localhost:3001", token=os.environ["OMS_TRADING_TOKEN"])

pf = oms.portfolios()[0]                      # what am I allowed to trade?
oid = oms.submit(portfolio=pf.portfolio_id,
                 symbol="SPY260918C00770000@OPRA",
                 side="buy", quantity=1)
order = oms.wait_for(oid)                     # polls until terminal
print(order.status, order.avg_px)

for row in oms.orders(status="routed"):       # the blotter
    print(row.order_id, row.instrument_symbol, row.cum_qty)

oms.orders().to_pandas()                      # needs the [pandas] extra
```

Naming an instrument works three ways: `symbol="SPY260918C00770000@OPRA"`,
`symbol=... , venue=...`, or `instrument_id="2972271"`. A bare symbol resolves only
when it is unique across venues — many equity tickers are listed under several
exchange MICs, and the server answers 422 naming the candidates.

## CLI

```bash
export OMS_URL=http://localhost:3001
export OMS_TRADING_TOKEN=ak_....sk_....

oms portfolios
oms orders list --status routed --limit 20
oms orders get <order_id>
oms orders cancel <order_id>
oms submit --portfolio <id> --symbol SPY260918C00770000@OPRA --side buy --qty 1 --wait
oms positions <portfolio_id>
```

Add `--json` to any command to get raw JSON for piping into `jq`.

## Behaviour worth knowing

These follow from the server's contract, not from choices made here:

- **`submit` returns only an order id.** The API answers 204 with an empty body, so
  the client generates the id. Call `order()` or `wait_for()` to see the outcome.
- **Retrying a submit is safe.** `order_id` is the idempotency key, so a repeat is a
  409 — which this client treats as "already accepted" rather than an error. Never
  resubmit under a *new* id after a failure; a 502 means the OMS kept the order and
  only the broker leg failed.
- **`cancel` can be asynchronous.** It returns `"canceled"` when the OMS cancelled it
  outright, or `"pending"` when the request went to the broker and will be confirmed
  later. `"pending"` does not mean cancelled.
- **`wait_for` polls.** The OMS has no SSE or WebSocket channel for order events.
- **Errors are plain text**, mapped to typed exceptions (`Rejected`, `Forbidden`,
  `BrokerRejected`, …), all subclasses of `OMSError`, each carrying `.status` and
  `.message`.

## Tests

```bash
pip install -e "clients/python[dev]" && pytest clients/python
```

No server needed — the transport is stubbed.
