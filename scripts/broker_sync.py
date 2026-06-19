#!/usr/bin/env python3
"""
Sync broker symbology into the master from a broker's catalog (Alpaca, equities).

Where seed_instruments.py mints the master instrument universe from a *data*
provider (Databento), this maps each master instrument to a *broker*'s own
instrument — broker symbol, native id (the handle orders route on), and
tradability — so the OMS can resolve instrument_id -> broker handle at order time
(`instrument_id -> broker_instrument[broker] -> native_id -> broker API`).

Reference-data ingestion, not execution: it writes ods.public.broker_instrument
as market_user. The OMS only *reads* this table. Match is by symbol against the
master SPOT universe (unambiguous for US equities); the broker's exchange code is
stored as-is (it's the broker's naming, not the canonical MIC).

Examples:
  export ALPACA_PAPER_API_KEY="..." ALPACA_PAPER_API_SECRET="..."   # ALPACA_ENV=PAPER (default)
  export DB_HOST=localhost DB_PORT=5432 ODS_DB=ods DB_USER=market_user DB_PASSWORD=...

  python broker_sync.py --dry-run     # fetch + match, print, no write
  python broker_sync.py               # upsert broker_instrument
"""

import argparse
import logging
import os
import sys

import psycopg
import requests
from dotenv import load_dotenv

logging.basicConfig(
    level=logging.INFO,
    stream=sys.stdout,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%Y-%m-%dT%H:%M:%S",
)
log = logging.getLogger("airflow.task")

load_dotenv()

BROKER_CODE = "ALPACA"
INSTRUMENT_CLASS = "SPOT"  # equities; options are a follow-up (Alpaca /v2/options/contracts)
ALPACA_BASE = {
    "PAPER": "https://paper-api.alpaca.markets",
    "LIVE": "https://api.alpaca.markets",
}


def get_ods_conn() -> psycopg.Connection:
    """Connect to the `ods` database (master data) as market_user."""
    user = os.environ.get("DB_USER")
    if not user:
        raise RuntimeError("Missing DB_USER environment variable.")
    return psycopg.connect(
        host=os.environ.get("DB_HOST", "localhost"),
        port=int(os.environ.get("DB_PORT", "5432")),
        dbname=os.environ.get("ODS_DB", "ods"),
        user=user,
        password=os.environ.get("DB_PASSWORD"),
        autocommit=True,
    )


def load_instrument_map(conn: psycopg.Connection, instrument_class: str) -> dict[str, int]:
    """symbol -> master instrument_id, for the universe we want broker mappings for."""
    rows = conn.execute(
        "SELECT symbol, id FROM instrument WHERE instrument_class = %s",
        (instrument_class,),
    ).fetchall()
    return {r[0]: r[1] for r in rows}


def fetch_alpaca_assets() -> list[dict]:
    """Active, tradeable US-equity assets from Alpaca's catalog."""
    env = os.environ.get("ALPACA_ENV", "PAPER").upper()
    base = ALPACA_BASE.get(env)
    if base is None:
        raise RuntimeError(f"ALPACA_ENV must be PAPER or LIVE, got {env!r}")
    # Per-environment creds, matching the OMS naming (ALPACA_PAPER_*, ALPACA_LIVE_*).
    key = os.environ.get(f"ALPACA_{env}_API_KEY")
    secret = os.environ.get(f"ALPACA_{env}_API_SECRET")
    if not key or not secret:
        raise RuntimeError(
            f"Missing ALPACA_{env}_API_KEY / ALPACA_{env}_API_SECRET environment variables."
        )
    resp = requests.get(
        f"{base}/v2/assets",
        headers={"APCA-API-KEY-ID": key, "APCA-API-SECRET-KEY": secret},
        params={"status": "active", "asset_class": "us_equity"},
        timeout=60,
    )
    resp.raise_for_status()
    return resp.json()


def _min_qty(asset: dict):
    v = asset.get("min_order_size")
    try:
        return float(v) if v is not None else None
    except (TypeError, ValueError):
        return None


def map_rows(assets: list[dict], id_map: dict[str, int]) -> list[dict]:
    """Alpaca assets -> broker_instrument rows, keeping only symbols in the master."""
    rows = []
    for a in assets:
        instrument_id = id_map.get(a.get("symbol"))
        if instrument_id is None:
            continue
        rows.append({
            "instrument_id": instrument_id,
            "broker_code": BROKER_CODE,
            "broker_symbol": a["symbol"],
            "broker_exchange": a.get("exchange"),
            "native_id": a.get("id"),               # Alpaca asset UUID — the routing handle
            "is_tradeable": bool(a.get("tradable")),
            "min_quantity": _min_qty(a),
        })
    return rows


def upsert(conn: psycopg.Connection, rows: list[dict]) -> int:
    if not rows:
        return 0
    with conn.cursor() as cur:
        cur.executemany(
            """
            INSERT INTO broker_instrument
                (instrument_id, broker_code, broker_symbol, broker_exchange,
                 native_id, is_tradeable, min_quantity)
            VALUES
                (%(instrument_id)s, %(broker_code)s, %(broker_symbol)s, %(broker_exchange)s,
                 %(native_id)s, %(is_tradeable)s, %(min_quantity)s)
            ON CONFLICT (instrument_id, broker_code) DO UPDATE SET
                broker_symbol = EXCLUDED.broker_symbol,
                broker_exchange = EXCLUDED.broker_exchange,
                native_id = EXCLUDED.native_id,
                is_tradeable = EXCLUDED.is_tradeable,
                min_quantity = EXCLUDED.min_quantity,
                updated_at = now()
            """,
            rows,
        )
    return len(rows)


def main():
    p = argparse.ArgumentParser(description="Sync broker_instrument from Alpaca's catalog.")
    p.add_argument("--dry-run", action="store_true", help="Fetch + match, print, no write")
    args = p.parse_args()

    conn = get_ods_conn()
    try:
        id_map = load_instrument_map(conn, INSTRUMENT_CLASS)
        log.info(f"Loaded {len(id_map)} {INSTRUMENT_CLASS} instrument(s) from ods master")

        assets = fetch_alpaca_assets()
        log.info(f"Fetched {len(assets)} active {BROKER_CODE} us_equity asset(s)")

        rows = map_rows(assets, id_map)
        log.info(f"Matched {len(rows)} asset(s) to master instruments")

        if args.dry_run:
            for r in rows[:20]:
                log.info(f"  {r['broker_symbol']} -> instrument_id={r['instrument_id']} "
                         f"native_id={r['native_id']} tradeable={r['is_tradeable']}")
            log.info("Dry run complete. No write.")
            return

        n = upsert(conn, rows)
        log.info(f"broker_sync done: upserted {n} {BROKER_CODE} broker_instrument row(s)")
    finally:
        conn.close()


if __name__ == "__main__":
    main()
