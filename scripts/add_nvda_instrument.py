#!/usr/bin/env python3
"""
add_nvda_instrument.py — create NVDA in oms_instrument and map it to Alpaca.

Reads OMS_ADMIN_TOKEN (and optionally OMS_BASE_URL) from the project root .env.

Usage:
    python scripts/add_nvda_instrument.py
"""

import os
import sys
from pathlib import Path

import requests


def load_env(path: Path) -> dict:
    env = {}
    if not path.exists():
        return env
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" in line:
            key, _, value = line.partition("=")
            env[key.strip()] = value.split("#")[0].strip()
    return env


def post(url: str, token: str, payload: dict) -> dict:
    resp = requests.post(url, json=payload, headers={"Authorization": f"Bearer {token}"})
    if not resp.ok:
        print(f"  ERROR {resp.status_code}: {resp.text}")
        sys.exit(1)
    return resp.json()


env_file = Path(__file__).parent.parent / ".env"
env = load_env(env_file)

token = os.environ.get("OMS_ADMIN_TOKEN") or env.get("OMS_ADMIN_TOKEN")
if not token:
    print("ERROR: OMS_ADMIN_TOKEN not set in environment or project root .env")
    sys.exit(1)

base = os.environ.get("OMS_BASE_URL") or env.get("OMS_BASE_URL", "http://localhost:3001")

# 1. Create master instrument
print("[1] Creating NVDA instrument...")
instrument = post(f"{base}/admin/instruments", token, {
    "code":        "NVDA_US_EQ",
    "symbol":      "NVDA",
    "asset_class": "EQUITY",
    "name":        "NVIDIA Corporation",
    "currency":    "USD",
    "exchange":    "NASDAQ",
    "status":      "ACTIVE",
})
print(f"  instrument created  id={instrument['id']}  code={instrument['code']}")

# 2. Create Alpaca broker mapping
print("\n[2] Creating Alpaca broker mapping for NVDA...")
mapping = post(f"{base}/admin/broker-instruments", token, {
    "instrument_id": instrument["id"],
    "broker_code":   "ALPACA",
    "broker_symbol": "NVDA",
    "is_tradeable":  True,
})
print(f"  mapping created     id={mapping['id']}  broker_symbol={mapping['broker_symbol']}")

print("\n" + "=" * 60)
print("Add this to scripts/.env to use with send_test_order.py:")
print(f"  OMS_INSTRUMENT_ID={instrument['id']}")
print("=" * 60)
