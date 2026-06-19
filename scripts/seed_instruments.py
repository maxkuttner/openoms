#!/usr/bin/env python3
"""
Seed the master `instrument` universe from Databento instrument definitions.

For each watchlist underlying this fetches the Databento `definition` schema for
the equity and (optionally) its OPRA option chain, maps it onto the mdm master
schema, and upserts into ods.public.instrument (+ instrument_derivative).

mdm owns the schema; this is the v1 single-source loader (Databento) that writes
directly. It connects to the `ods` database as market_user (granted write on the
instrument tables in mdm/access/ods.sql), distinct from the mds connection the
ohlcv loaders use.

Examples:
  export DATABENTO_API_KEY="..."
  export DB_HOST=localhost DB_PORT=5432 ODS_DB=ods DB_USER=market_user DB_PASSWORD=...

  python seed_instruments.py --symbol SPY --dry-run        # cost estimate only
  python seed_instruments.py --symbol SPY --equities-only  # just the equity row
  python seed_instruments.py --symbol SPY                  # equity + option chain
"""

import argparse
import logging
import math
import os
import sys
from datetime import date, datetime, timedelta, timezone

import databento as db
import pandas as pd
import psycopg
from databento.common.error import BentoClientError
from dotenv import load_dotenv

logging.basicConfig(
    level=logging.INFO,
    stream=sys.stdout,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%Y-%m-%dT%H:%M:%S",
)
log = logging.getLogger("airflow.task")

load_dotenv()

# Datasets. Equity dataset is configurable — confirm against the Databento plan
# (ARCX.PILLAR matches the existing SPY equity pipeline; a consolidated dataset
# like DBEQ.BASIC covers a broader universe).
EQUITY_DATASET = os.environ.get("DBN_EQUITY_DATASET", "ARCX.PILLAR")
OPTION_DATASET = os.environ.get("DBN_OPTION_DATASET", "OPRA.PILLAR")

# provider_code recorded in provider_instrument for everything this loader seeds.
PROVIDER_CODE = "DATABENTO"

# Optional EXCHANGE-code → venue MIC overrides, for cases where Databento's
# `exchange` field isn't itself a seeded MIC (notably the OPRA SIP). Empty by
# default; populate after inspecting real definition data.
EXCHANGE_TO_VENUE = {
    # "OPRA": "XOPR",
}


def get_client() -> db.Historical:
    if not os.environ.get("DATABENTO_API_KEY"):
        raise RuntimeError("Missing DATABENTO_API_KEY environment variable.")
    return db.Historical()


def get_ods_conn() -> psycopg.Connection:
    """Connect to the `ods` database (master data) as market_user."""
    host = os.environ.get("DB_HOST", "localhost")
    port = os.environ.get("DB_PORT", "5432")
    name = os.environ.get("ODS_DB", "ods")
    user = os.environ.get("DB_USER")
    password = os.environ.get("DB_PASSWORD")
    if not user:
        raise RuntimeError("Missing DB_USER environment variable.")
    return psycopg.connect(
        host=host, port=int(port), dbname=name, user=user, password=password,
        autocommit=True,
    )


# --- value coercion (robust to to_df pretty/raw forms) --------------------


def _num(v):
    if v is None:
        return None
    try:
        f = float(v)
    except (TypeError, ValueError):
        return None
    if math.isnan(f):
        return None
    return f


def _price(v):
    """A price/tick that may arrive pretty (0.01) or fixed-point 1e-9 (10000000)."""
    f = _num(v)
    if f is None:
        return None
    if abs(f) >= 1e6:            # clearly still 1e-9 fixed point
        f = f / 1e9
    return f


def _to_date(v):
    """expiration/activation -> date, or None if unset.

    In the pretty `to_df` form unset timestamps arrive as NaT and real ones as
    pandas Timestamps; in the raw form they're ns-since-epoch ints with an
    out-of-range sentinel for "unset". pd.isna() catches NaT/None/NaN; the
    year bound rejects the raw sentinel (which lands far in the future).
    """
    if pd.isna(v):
        return None
    if hasattr(v, "to_pydatetime"):     # pandas Timestamp
        try:
            d = v.to_pydatetime().date()
        except (ValueError, OverflowError):
            return None
    else:
        f = _num(v)
        if f is None or f <= 0:
            return None
        try:
            d = datetime.fromtimestamp(f / 1e9, tz=timezone.utc).date()
        except (ValueError, OverflowError, OSError):
            return None
    return d if d.year <= 2100 else None


def _decimals(tick):
    """Decimal places implied by a tick size, e.g. 0.01 -> 2, 1 -> 0."""
    if not tick or tick <= 0:
        return 2
    d = 0
    x = tick
    while abs(x - round(x)) > 1e-12 and d < 12:
        x *= 10
        d += 1
    return d


def _cls(raw):
    """Normalize Databento instrument_class to (instrument_class, option_kind)."""
    s = str(raw).strip().upper()
    if s in ("K", "STOCK", "EQUITY"):
        return "SPOT", None
    if s in ("C", "CALL"):
        return "OPTION", "CALL"
    if s in ("P", "PUT"):
        return "OPTION", "PUT"
    if s in ("F", "FUTURE"):
        return "FUTURE", None
    return None, None       # unsupported in v1 (spreads, fx, …)


def _venue(exchange):
    code = str(exchange or "").strip()
    return EXCHANGE_TO_VENUE.get(code, code)


# --- Databento fetch ------------------------------------------------------

def _range_for(client, dataset, days=3):
    """A small query window ending at the dataset's last available date.

    Databento historical data lags real time, so we can't use `today` as the
    end — query it from the dataset range and look back a few days to land on
    the most recent definition snapshot.
    """
    r = client.metadata.get_dataset_range(dataset=dataset)
    end_raw = r["end"] if isinstance(r, dict) else getattr(r, "end", r)
    end = datetime.fromisoformat(str(end_raw).replace("Z", "+00:00")).date()
    return (end - timedelta(days=days)), end


def estimate_cost(client, dataset, symbols, stype_in):
    start, end = _range_for(client, dataset)
    try:
        return float(client.metadata.get_cost(
            dataset=dataset, symbols=symbols, stype_in=stype_in,
            schema="definition", start=start.isoformat(), end=end.isoformat(),
        ))
    except BentoClientError as e:
        if "data_no_data_found_for_request" in str(e):
            return 0.0
        raise


def fetch_definitions(client, dataset, symbols, stype_in):
    """Return the latest definition snapshot per raw_symbol as list[dict]."""
    start, end = _range_for(client, dataset)
    try:
        store = client.timeseries.get_range(
            dataset=dataset, symbols=symbols, stype_in=stype_in,
            schema="definition", start=start.isoformat(), end=end.isoformat(),
        )
        df = store.to_df()
    except BentoClientError as e:
        if "data_no_data_found_for_request" in str(e):
            log.info(f"No definitions for {symbols} on {dataset} in {start}..{end}")
            return []
        raise
    if df.empty:
        return []
    df = df.reset_index()
    # keep the most recent definition per raw_symbol
    if "raw_symbol" not in df.columns:
        raise RuntimeError(f"No raw_symbol in definition columns: {list(df.columns)}")
    records = df.to_dict("records")
    latest = {}
    for r in records:
        latest[r.get("raw_symbol")] = r       # records are in ascending ts order
    return list(latest.values())


# --- mapping to mdm rows --------------------------------------------------

def map_instrument(rec, asset_class="EQUITY"):
    """Definition record -> instrument row dict, or None if unsupported."""
    iclass, _ = _cls(rec.get("instrument_class"))
    if iclass is None:
        return None
    symbol = str(rec.get("raw_symbol") or "").strip()
    venue = _venue(rec.get("exchange"))
    if not symbol or not venue:
        return None
    tick = _price(rec.get("min_price_increment")) or 0.01
    contract = (_num(rec.get("contract_multiplier"))
                or _price(rec.get("unit_of_measure_qty")) or 1)
    lot = _num(rec.get("min_lot_size_round_lot"))
    return {
        "symbol": symbol,
        "venue": venue,
        "currency": str(rec.get("currency") or "USD").strip().upper(),
        "asset_class": asset_class,
        "instrument_class": iclass,
        "name": symbol,                         # Databento defs carry no long name
        "price_precision": _decimals(tick),
        "size_precision": 0,
        "price_increment": tick,
        "size_increment": lot if (lot and lot > 0) else 1,
        "lot_size": lot if (lot and lot > 0) else None,
        "contract_size": contract if contract and contract > 0 else 1,
        # provider_instrument fields (Databento's view of this instrument)
        "native_id": (str(int(rec["instrument_id"]))
                      if rec.get("instrument_id") is not None else None),
        "provider_exchange": str(rec.get("exchange") or "").strip() or None,
    }


def map_derivative(rec):
    """Definition record -> instrument_derivative row dict (options only)."""
    iclass, option_kind = _cls(rec.get("instrument_class"))
    if iclass != "OPTION":
        return None
    symbol = str(rec.get("raw_symbol") or "").strip()
    venue = _venue(rec.get("exchange"))
    underlying = str(rec.get("underlying") or rec.get("asset") or "").strip() or None
    if not symbol or not venue or not underlying:
        return None
    return {
        "symbol": symbol,
        "venue": venue,
        "underlying_symbol": underlying,
        "option_kind": option_kind,
        "strike_price": _price(rec.get("strike_price")),
        "expiry_date": _to_date(rec.get("expiration")),
        "activation_date": _to_date(rec.get("activation")),
    }


# --- upsert ---------------------------------------------------------------

INSTRUMENT_COLS = [
    "symbol", "venue", "currency", "asset_class", "instrument_class", "name",
    "price_precision", "size_precision", "price_increment", "size_increment",
    "lot_size", "contract_size",
]
DERIVATIVE_COLS = [
    "symbol", "venue", "underlying_symbol", "option_kind",
    "strike_price", "expiry_date", "activation_date",
]
# Staged columns = instrument columns + the extras needed to also build the
# Databento provider_instrument rows (joined back to instrument by symbol+venue).
STAGE_COLS = INSTRUMENT_COLS + ["native_id", "provider_exchange"]


def upsert(conn, instruments, derivatives):
    with conn.transaction(), conn.cursor() as cur:
        cur.execute("SET LOCAL temp_buffers = '64MB'")

        # --- instruments: stage -> upsert on (symbol, venue), skipping rows
        # whose venue/currency FK target isn't seeded.
        cur.execute(
            "CREATE TEMP TABLE _stage_instrument ("
            "symbol text, venue text, currency text, asset_class text,"
            "instrument_class text, name text, price_precision int,"
            "size_precision int, price_increment numeric, size_increment numeric,"
            "lot_size numeric, contract_size numeric,"
            "native_id text, provider_exchange text) ON COMMIT DROP"
        )
        with cur.copy(
            "COPY _stage_instrument (" + ",".join(STAGE_COLS) + ") FROM STDIN"
        ) as copy:
            for r in instruments:
                copy.write_row(tuple(r[c] for c in STAGE_COLS))

        cur.execute(
            """
            WITH ins AS (
                INSERT INTO instrument
                    (symbol, venue, currency, asset_class, instrument_class, name,
                     price_precision, size_precision, price_increment,
                     size_increment, lot_size, contract_size)
                SELECT s.symbol, s.venue, s.currency, s.asset_class,
                       s.instrument_class, s.name, s.price_precision,
                       s.size_precision, s.price_increment, s.size_increment,
                       s.lot_size, s.contract_size
                FROM _stage_instrument s
                WHERE s.venue IN (SELECT code FROM venue)
                  AND s.currency IN (SELECT code FROM currency)
                ON CONFLICT (symbol, venue) DO UPDATE SET
                    currency = EXCLUDED.currency,
                    asset_class = EXCLUDED.asset_class,
                    instrument_class = EXCLUDED.instrument_class,
                    name = EXCLUDED.name,
                    price_precision = EXCLUDED.price_precision,
                    size_precision = EXCLUDED.size_precision,
                    price_increment = EXCLUDED.price_increment,
                    size_increment = EXCLUDED.size_increment,
                    lot_size = EXCLUDED.lot_size,
                    contract_size = EXCLUDED.contract_size,
                    updated_at = now()
                RETURNING 1
            )
            SELECT count(*) FROM ins
            """
        )
        upserted = cur.fetchone()[0]
        skipped = len(instruments) - upserted

        # --- provider_instrument: record Databento's symbology for each staged
        # instrument that exists in the master (data-side mirror of broker_sync's
        # broker_instrument). Join back to instrument by the (symbol, venue) key.
        cur.execute(
            """
            WITH ins AS (
                INSERT INTO provider_instrument
                    (instrument_id, provider_code, provider_symbol,
                     provider_exchange, native_id)
                SELECT i.id, %s, s.symbol, s.provider_exchange, s.native_id
                FROM _stage_instrument s
                JOIN instrument i ON i.symbol = s.symbol AND i.venue = s.venue
                ON CONFLICT (instrument_id, provider_code) DO UPDATE SET
                    provider_symbol = EXCLUDED.provider_symbol,
                    provider_exchange = EXCLUDED.provider_exchange,
                    native_id = EXCLUDED.native_id,
                    updated_at = now()
                RETURNING 1
            )
            SELECT count(*) FROM ins
            """,
            (PROVIDER_CODE,),
        )
        provider_upserted = cur.fetchone()[0]

        # --- derivatives: stage -> upsert, resolving instrument_id (the option)
        # and underlying_id (the equity) entirely in SQL.
        deriv_upserted = 0
        if derivatives:
            cur.execute(
                "CREATE TEMP TABLE _stage_derivative ("
                "symbol text, venue text, underlying_symbol text, option_kind text,"
                "strike_price numeric, expiry_date date, activation_date date)"
                " ON COMMIT DROP"
            )
            with cur.copy(
                "COPY _stage_derivative (" + ",".join(DERIVATIVE_COLS) + ") FROM STDIN"
            ) as copy:
                for r in derivatives:
                    copy.write_row(tuple(r[c] for c in DERIVATIVE_COLS))

            cur.execute(
                """
                WITH ins AS (
                    INSERT INTO instrument_derivative
                        (instrument_id, underlying_id, underlying_symbol,
                         option_kind, strike_price, expiry_date, activation_date)
                    SELECT i.id, u.id, s.underlying_symbol, s.option_kind,
                           s.strike_price, s.expiry_date, s.activation_date
                    FROM _stage_derivative s
                    JOIN instrument i ON i.symbol = s.symbol AND i.venue = s.venue
                    LEFT JOIN instrument u
                           ON u.symbol = s.underlying_symbol
                          AND u.instrument_class = 'SPOT'
                    ON CONFLICT (instrument_id) DO UPDATE SET
                        underlying_id = EXCLUDED.underlying_id,
                        underlying_symbol = EXCLUDED.underlying_symbol,
                        option_kind = EXCLUDED.option_kind,
                        strike_price = EXCLUDED.strike_price,
                        expiry_date = EXCLUDED.expiry_date,
                        activation_date = EXCLUDED.activation_date
                    RETURNING 1
                )
                SELECT count(*) FROM ins
                """
            )
            deriv_upserted = cur.fetchone()[0]

    return upserted, skipped, deriv_upserted, provider_upserted


# --- main -----------------------------------------------------------------

def parse_args():
    p = argparse.ArgumentParser(description="Seed instrument universe from Databento.")
    p.add_argument("--symbol", default="SPY", help="Underlying symbol")
    p.add_argument("--equities-only", action="store_true", help="Skip option chains")
    p.add_argument("--dry-run", action="store_true", help="Estimate cost only; no fetch/write")
    p.add_argument("--max-cost", type=float, help="Abort if estimated cost exceeds this")
    return p.parse_args()


def main():
    args = parse_args()
    symbol = args.symbol.upper()
    parent = f"{symbol}.OPT"
    client = get_client()

    # cost estimate (equity + option chain)
    cost = estimate_cost(client, EQUITY_DATASET, [symbol], "raw_symbol")
    if not args.equities_only:
        cost += estimate_cost(client, OPTION_DATASET, [parent], "parent")
    log.info(f"Estimated Databento cost: ${cost:.4f}")
    if args.max_cost is not None and cost > args.max_cost:
        raise RuntimeError(f"Estimated cost ${cost:.4f} exceeds --max-cost ${args.max_cost}")
    if args.dry_run:
        log.info("Dry run complete. No fetch or write.")
        return

    # equity definition
    equity_defs = fetch_definitions(client, EQUITY_DATASET, [symbol], "raw_symbol")
    instruments = [m for m in (map_instrument(r, "EQUITY") for r in equity_defs) if m]
    derivatives = []

    # option-chain definitions
    if not args.equities_only:
        opt_defs = fetch_definitions(client, OPTION_DATASET, [parent], "parent")
        for r in opt_defs:
            mi = map_instrument(r, "EQUITY")     # equity option -> asset_class EQUITY
            md = map_derivative(r)
            if mi and md:
                instruments.append(mi)
                derivatives.append(md)

    if not instruments:
        log.info(f"No mappable instruments for {symbol}; nothing to upsert.")
        return

    log.info(f"Upserting {len(instruments)} instrument(s), {len(derivatives)} derivative(s)...")
    conn = get_ods_conn()
    try:
        upserted, skipped, deriv, provider = upsert(conn, instruments, derivatives)
    finally:
        conn.close()
    log.info(
        f"seed_instruments done for {symbol}: instruments upserted={upserted} "
        f"skipped(FK)={skipped}; derivatives upserted={deriv}; "
        f"provider_instrument upserted={provider}"
    )
    if skipped:
        log.warning(
            f"{skipped} instrument(s) skipped — venue/currency not seeded. "
            "Check the exchange→MIC mapping (EXCHANGE_TO_VENUE) and that venues are seeded."
        )


if __name__ == "__main__":
    main()
