#!/usr/bin/env python3
"""
submit_order.py — submit a test order to the OMS.

Usage:
    python scripts/submit_order.py \
        --key-id ak_... \
        --secret sk_... \
        --account-id <uuid> \
        --book-id <uuid> \
        --symbol NVDA \
        --side buy \
        --qty 1 \
        --order-type market

    # Limit order:
    python scripts/submit_order.py \
        --key-id ak_... \
        --secret sk_... \
        --account-id <uuid> \
        --book-id <uuid> \
        --symbol NVDA \
        --side buy \
        --qty 1 \
        --order-type limit \
        --limit-price 900.00

    # Override base URL (default: http://localhost:3001):
    python scripts/submit_order.py --base-url http://localhost:3001 ...

Dependencies:
    pip install requests
"""

import argparse
import json
import sys
import uuid

import requests


def main():
    parser = argparse.ArgumentParser(description="Submit an order to the OMS")
    parser.add_argument("--key-id", required=True, dest="key_id", help="API key ID (ak_...)")
    parser.add_argument("--secret", required=True, help="API secret (sk_...)")
    parser.add_argument("--base-url", default="http://localhost:3001")
    parser.add_argument("--account-id", required=True, help="UUID of the oms_account to route to")
    parser.add_argument("--book-id", required=True, help="UUID of the oms_book")
    parser.add_argument("--symbol", required=True, help="Ticker symbol, e.g. NVDA")
    parser.add_argument("--side", required=True, choices=["buy", "sell"])
    parser.add_argument("--qty", required=True, type=float, help="Order quantity")
    parser.add_argument(
        "--order-type", required=True, choices=["market", "limit"], dest="order_type"
    )
    parser.add_argument(
        "--limit-price", type=float, default=None, dest="limit_price",
        help="Required when --order-type=limit"
    )
    parser.add_argument(
        "--tif", default="day", choices=["day", "gtc", "ioc", "fok"],
        help="Time in force (default: day)"
    )
    parser.add_argument("--order-id", default=None, dest="order_id")
    parser.add_argument("--client-order-id", default=None, dest="client_order_id")
    args = parser.parse_args()

    if args.order_type == "limit" and args.limit_price is None:
        print("ERROR: --limit-price is required when --order-type=limit")
        sys.exit(1)

    order_id = args.order_id or str(uuid.uuid4())
    client_order_id = args.client_order_id or str(uuid.uuid4())

    payload = {
        "order_id": order_id,
        "client_order_id": client_order_id,
        "book_id": args.book_id,
        "account_id": args.account_id,
        "instrument_id": args.symbol,
        "side": args.side,
        "order_type": args.order_type,
        "time_in_force": args.tif,
        "quantity": args.qty,
        "limit_price": args.limit_price,
    }

    print("Submitting order:")
    print(json.dumps(payload, indent=2))
    print()

    url = f"{args.base_url}/orders/submit"
    resp = requests.post(url, json=payload, auth=(args.key_id, args.secret))

    if resp.status_code == 204:
        print("OK — order routed successfully")
        print(f"  order_id        = {order_id}")
        print(f"  client_order_id = {client_order_id}")
    else:
        print(f"ERROR {resp.status_code}: {resp.text}")
        sys.exit(1)


if __name__ == "__main__":
    main()
