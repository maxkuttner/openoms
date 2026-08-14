#!/usr/bin/env python3
"""End-to-end walkthrough of the OMS Python client against a running OMS.

Exercises every method on the client, then sends one real order and cancels it.

The order is a **limit buy priced well below the market**, on purpose. A market
order fills immediately and cannot be cancelled, which would make the cancel half of
this script untestable; a resting bid sits in the book until we take it back. Crypto
is the default target because it trades 24/7 — the Alpaca options path only accepts
market orders during US market hours, so it cannot be exercised at 4am.

Orders route to Binance **testnet** when the broker connection's environment is
PAPER, so nothing here touches real money. Note the OMS deliberately marks crypto
against *production* prices even though it routes to testnet, so a mark and a fill
price can legitimately disagree.

Usage:
    export OMS_URL=http://localhost:3001
    export OMS_TRADING_TOKEN=ak_....sk_....

    python examples/smoke_test.py                    # BTCUSDT@BINANCE, ~12 USDT bid
    python examples/smoke_test.py --dry-run          # everything except submit/cancel
    python examples/smoke_test.py --symbol ETHUSDT@BINANCE --notional 20
    python examples/smoke_test.py --portfolio <uuid> # skip auto-selection
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request

from oms_client import OMS, OMSError, Rejected

# Where to ask what a coin is worth. Testnet, not production: the order rests in
# testnet's book, so testnet's price is the one that decides whether it rests.
TESTNET_TICKER = "https://testnet.binance.vision/api/v3/ticker/price?symbol={}"

# How far below the market to place the bid. Binance's PERCENT_PRICE_BY_SIDE filter
# rejects a bid below half the average price, so this stays comfortably inside that
# while being far enough out that nothing crosses it during the run.
DISCOUNT = 0.80


def step(n: int, title: str) -> None:
    print(f"\n\033[1m[{n}] {title}\033[0m")


def reference_price(symbol_at_venue: str) -> float:
    """Current testnet price for the bare symbol part of SYMBOL@VENUE."""
    symbol = symbol_at_venue.split("@")[0]
    with urllib.request.urlopen(TESTNET_TICKER.format(symbol), timeout=10) as resp:
        return float(json.load(resp)["price"])


def pick_portfolio(oms: OMS, explicit: str | None):
    portfolios = oms.portfolios()
    if not portfolios:
        sys.exit("error: this token has no portfolio grants — ask an admin for one")
    for p in portfolios:
        print(f"    {p.code:24} trade={p.can_trade} view={p.can_view} alloc={p.can_allocate}")
    if explicit:
        match = [p for p in portfolios if p.portfolio_id == explicit or p.code == explicit]
        if not match:
            sys.exit(f"error: {explicit} is not a portfolio this token can see")
        return match[0]
    tradeable = [p for p in portfolios if p.can_trade]
    if not tradeable:
        sys.exit("error: this token can view portfolios but not trade any (needs can_trade)")
    # Prefer a crypto-looking portfolio, since the default symbol is crypto.
    return next((p for p in tradeable if "crypto" in p.code.lower()), tradeable[0])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--url", default=os.environ.get("OMS_URL", "http://localhost:3001"))
    ap.add_argument("--symbol", default="BTCUSDT@BINANCE", help="SYMBOL@VENUE")
    ap.add_argument("--portfolio", help="portfolio id or code (default: first tradeable)")
    ap.add_argument("--notional", type=float, default=12.0, help="order size in quote ccy")
    ap.add_argument("--price", type=float, help="explicit limit price (skips the lookup)")
    ap.add_argument("--dry-run", action="store_true", help="read-only; no order is sent")
    args = ap.parse_args()

    token = os.environ.get("OMS_TRADING_TOKEN")
    if not token:
        sys.exit("error: set OMS_TRADING_TOKEN (mint one via POST /admin/trading-tokens)")

    oms = OMS(args.url, token)

    step(1, "Portfolios this token may act on")
    portfolio = pick_portfolio(oms, args.portfolio)
    print(f"  -> using {portfolio.code} ({portfolio.portfolio_id})")

    step(2, "Current positions")
    positions = oms.positions(portfolio.portfolio_id)
    if not positions:
        print("  (none)")
    for p in positions:
        mark = "-" if p.mark is None else f"{p.mark:,.2f}"
        print(f"  instrument {p.instrument_id:>10}  qty {p.net_qty:<12g} mark {mark}")

    step(3, "Blotter — most recent orders in these portfolios")
    recent = oms.orders(limit=5)
    if not recent:
        print("  (none)")
    for r in recent:
        print(f"  {r.order_id[:8]}  {str(r.instrument_symbol):22} {r.side:4} "
              f"{r.status:16} {r.cum_qty:g}/{r.original_qty:g}")

    step(4, "Error handling — an ambiguous symbol must be refused, legibly")
    try:
        oms.submit(portfolio=portfolio.portfolio_id, symbol="AAAU", side="buy", quantity=1)
        print("  !! expected a rejection and did not get one")
    except Rejected as err:
        print(f"  -> Rejected: {err.message}")
    except OMSError as err:
        # A token without an Alpaca-backed account cannot reach the ambiguity check;
        # that is fine, the point is that it refused rather than guessed a venue.
        print(f"  -> refused ({err.status}): {err.message}")

    # ── the live half ────────────────────────────────────────────────────────

    price = args.price or round(reference_price(args.symbol) * DISCOUNT, 2)
    qty = round(args.notional / price, 5)
    step(5, f"Submit a resting limit buy: {qty:g} {args.symbol} @ {price:,.2f}")
    print(f"  (market is ~{price / DISCOUNT:,.2f}; this bid sits {(1 - DISCOUNT) * 100:.0f}% "
          f"below it so it rests instead of filling)")
    if args.dry_run:
        print("  --dry-run: stopping before anything is sent")
        return 0

    try:
        order_id = oms.submit(
            portfolio=portfolio.portfolio_id,
            symbol=args.symbol,
            side="buy",
            quantity=qty,
            order_type="limit",
            limit_price=price,
            time_in_force="gtc",
            client_order_id="sdk-smoke-test",
        )
    except OMSError as err:
        sys.exit(f"error: submit refused ({err.status}): {err.message}")
    print(f"  -> order_id {order_id}")

    step(6, "Read it back")
    order = oms.order(order_id)
    print(f"  status={order.status} leaves={order.leaves_qty:g} "
          f"cum={order.cum_qty:g} version={order.version}")

    step(7, "Idempotency — resubmitting the same order_id is a no-op, not a duplicate")
    again = oms.submit(
        portfolio=portfolio.portfolio_id, symbol=args.symbol, side="buy", quantity=qty,
        order_type="limit", limit_price=price, time_in_force="gtc", order_id=order_id,
    )
    print(f"  -> returned the same id, no exception: {again == order_id}")

    step(8, "Confirm it shows on the blotter")
    mine = [r for r in oms.orders(limit=20) if r.order_id == order_id]
    print(f"  -> found: {bool(mine)}" + (f"  status={mine[0].status}" if mine else ""))

    step(9, "Cancel it")
    outcome = oms.cancel(order_id, reason="smoke test")
    print(f"  -> {outcome}" + ("  (forwarded to broker; confirmed asynchronously)"
                               if outcome == "pending" else "  (cancelled locally)"))

    step(10, "Wait for a terminal state")
    try:
        final = oms.wait_for(order_id, timeout=30, interval=1.0)
        print(f"  -> {final.status}  (filled {final.cum_qty:g} of {final.original_qty:g})")
        if final.status != "canceled":
            print(f"  !! expected 'canceled' — the resting bid may have been crossed")
    except TimeoutError as err:
        print(f"  !! {err}")
        print("     the cancel is still in flight; re-run `oms orders get` to follow it")

    print("\n\033[1mdone\033[0m — every client method exercised against a live OMS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
