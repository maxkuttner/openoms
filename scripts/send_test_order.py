#!/usr/bin/env python3
"""Send a test market order for 1 NVDA via Alpaca Paper."""

import json
import os
import sys
import uuid
import requests


def _load_env(path: str) -> dict:
    env = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, _, value = line.partition("=")
            env[key.strip()] = value.strip()
    return env


_here = os.path.dirname(os.path.abspath(__file__))
_env_path = os.path.join(_here, ".env")
if not os.path.exists(_env_path):
    print(f"ERROR: {_env_path} not found — copy scripts/.env.example and fill in your values")
    sys.exit(1)

_env = _load_env(_env_path)

KEY_ID     = _env["OMS_KEY_ID"]
SECRET     = _env["OMS_SECRET"]
ACCOUNT_ID = _env["OMS_ACCOUNT_ID"]
BOOK_ID    = _env["OMS_BOOK_ID"]
BASE_URL   = _env.get("OMS_BASE_URL", "http://localhost:3001")

payload = {
    "order_id":        str(uuid.uuid4()),
    "client_order_id": str(uuid.uuid4()),
    "book_id":         BOOK_ID,
    "account_id":      ACCOUNT_ID,
    "instrument_id":   "NVDA",
    "side":            "buy",
    "order_type":      "market",
    "time_in_force":   "day",
    "quantity":        1,
    "limit_price":     None,
}

print(json.dumps(payload, indent=2))
resp = requests.post(f"{BASE_URL}/orders/submit", json=payload, auth=(KEY_ID, SECRET))

if resp.status_code == 204:
    print(f"\nOK — order routed  order_id={payload['order_id']}")
else:
    print(f"\nERROR {resp.status_code}: {resp.text}")
    sys.exit(1)
