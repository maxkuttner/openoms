#!/usr/bin/env python3
"""Command-line front end for the OMS trading API — a blotter in a terminal.

Credentials come from the environment so no token is ever typed into a shell history:

    export OMS_URL=http://localhost:3001
    export OMS_TRADING_TOKEN=ak_....sk_....

Usage:
    oms portfolios
    oms positions <portfolio_id>
    oms orders list [--status routed] [--portfolio ID] [--limit 20]
    oms orders get <order_id>
    oms orders cancel <order_id> [--reason "..."]
    oms submit --portfolio ID --symbol SPY260918C00770000@OPRA --side buy --qty 1
    oms submit --portfolio ID --symbol AAPL --venue XNAS --side buy --qty 10 \
               --type limit --limit-price 190.00

Every command takes `--json` to emit raw JSON instead of a table, for piping into jq.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import sys
from typing import List, Sequence

from .client import OMS
from .errors import OMSError

ENV_URL = "OMS_URL"
ENV_TOKEN = "OMS_TRADING_TOKEN"


def _client(args: argparse.Namespace) -> OMS:
    url = args.url or os.environ.get(ENV_URL)
    token = os.environ.get(ENV_TOKEN)
    if not url:
        sys.exit(f"error: no server URL — set {ENV_URL} or pass --url")
    if not token:
        sys.exit(f"error: no trading token — set {ENV_TOKEN}")
    return OMS(url, token)


def _print_table(rows: Sequence, columns: List[str]) -> None:
    """Print dataclass rows as an aligned table, widening each column to fit."""
    if not rows:
        print("(no rows)")
        return
    data = [[_cell(getattr(r, c, None)) for c in columns] for r in rows]
    widths = [
        max(len(c), *(len(d[i]) for d in data)) for i, c in enumerate(columns)
    ]
    print("  ".join(c.upper().ljust(w) for c, w in zip(columns, widths)))
    for row in data:
        print("  ".join(v.ljust(w) for v, w in zip(row, widths)))


def _cell(value) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        # Trim the trailing zeros a raw float repr leaves on whole quantities.
        return f"{value:g}"
    return str(value)


def _emit(args: argparse.Namespace, rows, columns: List[str]) -> None:
    if args.json:
        print(json.dumps([dataclasses.asdict(r) for r in rows], indent=2))
    else:
        _print_table(rows, columns)


# ── commands ─────────────────────────────────────────────────────────────────


def cmd_portfolios(args: argparse.Namespace) -> None:
    _emit(
        args,
        _client(args).portfolios(),
        ["code", "name", "status", "can_trade", "can_view", "can_allocate", "portfolio_id"],
    )


def cmd_positions(args: argparse.Namespace) -> None:
    _emit(
        args,
        _client(args).positions(args.portfolio_id),
        ["instrument_id", "net_qty", "avg_cost", "mark", "market_value", "unrealized_pnl"],
    )


def cmd_orders_list(args: argparse.Namespace) -> None:
    rows = _client(args).orders(
        status=args.status,
        portfolio_id=args.portfolio,
        side=args.side,
        limit=args.limit,
    )
    _emit(
        args,
        rows,
        [
            "order_id",
            "instrument_symbol",
            "side",
            "order_type",
            "status",
            "original_qty",
            "cum_qty",
            "avg_px",
            "portfolio_code",
            "created_at",
        ],
    )


def cmd_orders_get(args: argparse.Namespace) -> None:
    order = _client(args).order(args.order_id)
    print(json.dumps(dataclasses.asdict(order), indent=2))


def cmd_orders_cancel(args: argparse.Namespace) -> None:
    outcome = _client(args).cancel(args.order_id, reason=args.reason)
    if outcome == "pending":
        print("cancel sent to broker — still working; poll `oms orders get` to confirm")
    else:
        print("canceled")


def cmd_submit(args: argparse.Namespace) -> None:
    oms = _client(args)
    order_id = oms.submit(
        portfolio=args.portfolio,
        symbol=args.symbol,
        venue=args.venue,
        instrument_id=args.instrument_id,
        side=args.side,
        quantity=args.qty,
        order_type=args.type,
        time_in_force=args.tif,
        limit_price=args.limit_price,
        account_id=args.account,
    )
    print(order_id)
    if args.wait:
        print(oms.wait_for(order_id).status)


# ── parser ───────────────────────────────────────────────────────────────────


def build_parser() -> argparse.ArgumentParser:
    # Global flags live on a parent every leaf command inherits, so they are accepted
    # where they read naturally — `oms orders list --json`, not `oms --json orders
    # list`. Defining them on the top-level parser instead would only allow the
    # latter, and argparse would reject the form the help text advertises.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--url", help=f"OMS base URL (default: ${ENV_URL})")
    common.add_argument("--json", action="store_true", help="emit raw JSON")

    p = argparse.ArgumentParser(prog="oms", description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="command", required=True)

    sub.add_parser(
        "portfolios", parents=[common], help="portfolios this token may act on"
    ).set_defaults(func=cmd_portfolios)

    pos = sub.add_parser("positions", parents=[common], help="open positions in a portfolio")
    pos.add_argument("portfolio_id")
    pos.set_defaults(func=cmd_positions)

    orders = sub.add_parser("orders", help="list, inspect, and cancel orders")
    osub = orders.add_subparsers(dest="orders_command", required=True)

    lst = osub.add_parser("list", parents=[common], help="the blotter")
    lst.add_argument("--status", help="filter by status (e.g. routed, filled)")
    lst.add_argument("--portfolio", help="filter by portfolio id")
    lst.add_argument("--side", choices=["buy", "sell"])
    lst.add_argument("--limit", type=int, default=100)
    lst.set_defaults(func=cmd_orders_list)

    get = osub.add_parser("get", parents=[common], help="one order's full state")
    get.add_argument("order_id")
    get.set_defaults(func=cmd_orders_get)

    can = osub.add_parser("cancel", parents=[common], help="cancel an order")
    can.add_argument("order_id")
    can.add_argument("--reason")
    can.set_defaults(func=cmd_orders_cancel)

    sb = sub.add_parser("submit", parents=[common], help="submit an order")
    sb.add_argument("--portfolio", required=True)
    sb.add_argument("--symbol", help="SYMBOL or SYMBOL@VENUE")
    sb.add_argument("--venue", help="venue MIC, when not embedded in --symbol")
    sb.add_argument("--instrument-id", dest="instrument_id", help="instead of --symbol")
    sb.add_argument("--side", required=True, choices=["buy", "sell"])
    sb.add_argument("--qty", required=True, type=float)
    sb.add_argument("--type", default="market", choices=["market", "limit"])
    sb.add_argument("--tif", default="day", choices=["day", "gtc", "ioc", "fok"])
    sb.add_argument("--limit-price", dest="limit_price", type=float)
    sb.add_argument("--account", help="override the portfolio's default account")
    sb.add_argument("--wait", action="store_true", help="poll until terminal")
    sb.set_defaults(func=cmd_submit)

    return p


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        args.func(args)
    except OMSError as err:
        # The server's message is the useful part; a traceback is not.
        sys.exit(f"error: {err.message} (HTTP {err.status})")
    except TimeoutError as err:
        sys.exit(f"error: {err}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
