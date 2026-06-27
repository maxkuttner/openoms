#!/usr/bin/env python3
"""
Seed the master `instrument` universe from Databento instrument definitions.

Universes are DB-backed selections (ods.public.instrument_universe + child
symbols): a dataset plus an optional symbol list (empty = whole dataset via
ALL_SYMBOLS). For each selected universe this fetches the Databento `definition`
schema, maps it to the master schema, and upserts instrument (+
instrument_derivative + provider_instrument), writing seed-state back onto the
universe row.

Examples:
  python seed_instruments.py                      # interactive picker (TTY)
  python seed_instruments.py --all-enabled        # seed every enabled universe
  python seed_instruments.py --universe SPY_OPTIONS --dry-run
"""

import argparse
import logging
import math
import os
import sys
from datetime import datetime, timedelta, timezone

import databento as db
import pandas as pd
import psycopg
import questionary
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

PROVIDER_CODE = "DATABENTO"

# Databento exchange-code → seeded venue MIC overrides, for when the exchange
# field isn't itself a seeded MIC. Empty by default.
EXCHANGE_TO_VENUE = {}


def get_client() -> db.Historical:
    if not os.environ.get("DATABENTO_API_KEY"):
        raise RuntimeError("Missing DATABENTO_API_KEY environment variable.")
    return db.Historical()


def get_ods_conn() -> psycopg.Connection:
    """Connect to the `ods` database as the configured DB_USER."""
    user = os.environ.get("DB_USER")
    if not user:
        raise RuntimeError("Missing DB_USER environment variable.")
    return psycopg.connect(
        host=os.environ.get("DB_HOST", "localhost"),
        port=int(os.environ.get("DB_PORT", "5432")),
        dbname=os.environ.get("ODS_DB", "ods"),
        user=user, password=os.environ.get("DB_PASSWORD"),
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
    return None if math.isnan(f) else f


def _price(v):
    """Tick/price; may arrive pretty (0.01) or 1e-9 fixed-point."""
    f = _num(v)
    if f is None:
        return None
    return f / 1e9 if abs(f) >= 1e6 else f


def _to_date(v):
    """expiration/activation → date, or None if unset/sentinel."""
    if pd.isna(v):
        return None
    if hasattr(v, "to_pydatetime"):
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
    """Decimal places implied by a tick size, e.g. 0.01 -> 2."""
    if not tick or tick <= 0:
        return 2
    d, x = 0, tick
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
    return None, None


def _venue(exchange):
    code = str(exchange or "").strip()
    return EXCHANGE_TO_VENUE.get(code, code)


# --- Databento fetch ------------------------------------------------------

def _range_for(client, dataset, days=3):
    """A small window ending at the dataset's last available date (historical lags realtime)."""
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
        df = client.timeseries.get_range(
            dataset=dataset, symbols=symbols, stype_in=stype_in,
            schema="definition", start=start.isoformat(), end=end.isoformat(),
        ).to_df()
    except BentoClientError as e:
        if "data_no_data_found_for_request" in str(e):
            log.info(f"No definitions for {symbols} on {dataset} in {start}..{end}")
            return []
        raise
    if df.empty:
        return []
    df = df.reset_index()
    if "raw_symbol" not in df.columns:
        raise RuntimeError(f"No raw_symbol in definition columns: {list(df.columns)}")
    latest = {}
    for r in df.to_dict("records"):     # ascending ts order → last write wins
        latest[r.get("raw_symbol")] = r
    return list(latest.values())


# --- mapping to master rows -----------------------------------------------

def map_instrument(rec):
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
        "asset_class": "EQUITY",
        "instrument_class": iclass,
        "name": symbol,                         # defs carry no long name
        "price_precision": _decimals(tick),
        "size_precision": 0,
        "price_increment": tick,
        "size_increment": lot if (lot and lot > 0) else 1,
        "lot_size": lot if (lot and lot > 0) else None,
        "contract_size": contract if contract and contract > 0 else 1,
        "native_id": (str(int(rec["instrument_id"]))
                      if rec.get("instrument_id") is not None else None),
        # non-null resolver key; fall back to venue when blank
        "provider_exchange": str(rec.get("exchange") or "").strip() or venue,
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
STAGE_COLS = INSTRUMENT_COLS + ["native_id", "provider_exchange"]


def upsert(conn, instruments, derivatives):
    with conn.transaction(), conn.cursor() as cur:
        cur.execute("SET LOCAL temp_buffers = '64MB'")

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

        # Skip rows whose venue/currency FK target isn't seeded.
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

        # provider_instrument: join staged rows back to instrument by (symbol, venue).
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


# --- universe catalog -----------------------------------------------------

def load_universes(conn, *, code=None, enabled_only=False):
    """Load universes (+ their child symbols) from the catalog."""
    where, params = [], []
    if code:
        where.append("u.code = %s"); params.append(code)
    if enabled_only:
        where.append("u.enabled = true")
    clause = (" WHERE " + " AND ".join(where)) if where else ""
    with conn.cursor() as cur:
        cur.execute(
            "SELECT u.code, u.description, u.category, u.dataset, u.option_dataset, "
            "u.stype_in, u.include_options, u.enabled, u.status, u.last_seeded_at "
            "FROM instrument_universe u" + clause + " ORDER BY u.category, u.code", params)
        cols = [d[0] for d in cur.description]
        universes = [dict(zip(cols, r)) for r in cur.fetchall()]
        for u in universes:
            cur.execute(
                "SELECT symbol FROM instrument_universe_symbol "
                "WHERE universe_code = %s ORDER BY symbol", (u["code"],))
            u["symbols"] = [r[0] for r in cur.fetchall()]
    return universes


def set_universe_enabled(conn, code, enabled):
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE instrument_universe SET enabled = %s, updated_at = now() "
            "WHERE code = %s", (enabled, code))


def update_universe_state(conn, code, status, *, count=None, error=None):
    """Write seed-state back onto the universe row; stamps last_seeded_at on SEEDED."""
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE instrument_universe SET status = %s, "
            "last_seeded_at = CASE WHEN %s = 'SEEDED' THEN now() ELSE last_seeded_at END, "
            "instrument_count = COALESCE(%s, instrument_count), "
            "last_error = %s, updated_at = now() WHERE code = %s",
            (status, status, count, error, code))


# --- per-universe seeding -------------------------------------------------

def _equity_symbols(u):
    return u["symbols"] if u["symbols"] else "ALL_SYMBOLS"


def estimate_spec_cost(client, u):
    if u["category"] != "EQUITY":
        return 0.0
    cost = estimate_cost(client, u["dataset"], _equity_symbols(u), u["stype_in"])
    if u["include_options"] and u["symbols"] and u["option_dataset"]:
        parents = [f"{s}.OPT" for s in u["symbols"]]
        cost += estimate_cost(client, u["option_dataset"], parents, "parent")
    return cost


def seed_spec(client, conn, u):
    """Fetch + map + upsert one universe. Returns (upserted, skipped, deriv, provider)."""
    if u["category"] != "EQUITY":
        log.warning(f"{u['code']}: {u['category']} datasets not seedable yet; skipping.")
        return (0, 0, 0, 0)

    defs = fetch_definitions(client, u["dataset"], _equity_symbols(u), u["stype_in"])
    instruments = [m for m in (map_instrument(r) for r in defs) if m]
    derivatives = []

    if u["include_options"]:
        if not u["symbols"]:
            log.warning(f"{u['code']}: include_options ignored — whole-dataset universe "
                        "has no explicit symbols (OPRA is per-underlying).")
        elif not u["option_dataset"]:
            log.warning(f"{u['code']}: include_options set but option_dataset is null.")
        else:
            parents = [f"{s}.OPT" for s in u["symbols"]]
            for r in fetch_definitions(client, u["option_dataset"], parents, "parent"):
                mi, md = map_instrument(r), map_derivative(r)
                if mi and md:
                    instruments.append(mi)
                    derivatives.append(md)

    if not instruments:
        log.info(f"{u['code']}: no mappable instruments.")
        return (0, 0, 0, 0)
    log.info(f"{u['code']}: upserting {len(instruments)} instrument(s), "
             f"{len(derivatives)} derivative(s)…")
    return upsert(conn, instruments, derivatives)


def estimate_universes(client, universes):
    """Estimate each universe's cost (free metadata-only calls). Returns (estimates, total)."""
    estimates = [(u, estimate_spec_cost(client, u)) for u in universes]
    return estimates, sum(c for _, c in estimates)


def print_cost_table(estimates, total):
    print("\nEstimated Databento cost (definition schema, free to estimate):\n")
    for u, cost in estimates:
        nsym = len(u["symbols"]) if u["symbols"] else "ALL"
        note = "  (not seedable in v1)" if u["category"] != "EQUITY" else ""
        print(f"  {u['code']:<16} {u['category']:<7} sym={str(nsym):<4} ${cost:>10.4f}{note}")
    print(f"  {'TOTAL':<30} ${total:>10.4f}\n")


def seed_universes(client, conn, universes, *, dry_run, max_cost, persist_state, confirm=False):
    """Estimate + show cost, gate, optionally confirm, then seed; record state."""
    estimates, total = estimate_universes(client, universes)
    print_cost_table(estimates, total)
    if max_cost is not None and total > max_cost:
        raise RuntimeError(f"Estimated cost ${total:.4f} exceeds --max-cost ${max_cost}")
    if dry_run:
        log.info(f"Dry run complete ({len(universes)} universe(s), ${total:.4f}). No fetch or write.")
        return
    if confirm and input(
            f"Seed {len(universes)} universe(s) for ~${total:.4f}? [y/N]: "
            ).strip().lower() != "y":
        log.info("Aborted.")
        return

    failures = []
    for u in universes:
        log.info(f"seeding universe {u['code']} …")
        if persist_state:
            update_universe_state(conn, u["code"], "SEEDING")
        try:
            upserted, skipped, deriv, provider = seed_spec(client, conn, u)
        except Exception as e:
            if persist_state:
                update_universe_state(conn, u["code"], "ERROR", error=str(e)[:500])
            log.error(f"{u['code']}: seeding failed: {e}")
            failures.append(u["code"])
            continue
        if persist_state:
            update_universe_state(conn, u["code"], "SEEDED", count=upserted, error=None)
        log.info(f"{u['code']} done: instruments upserted={upserted} skipped(FK)={skipped}; "
                 f"derivatives={deriv}; provider_instrument={provider}")
        if skipped:
            log.warning(f"{u['code']}: {skipped} instrument(s) skipped — venue/currency not seeded.")
    if failures:
        raise SystemExit(f"seeding failed for: {', '.join(failures)}")


def run_questionnaire(conn):
    """Checkbox-pick which universes are enabled, persist the change, return them."""
    universes = load_universes(conn)
    if not universes:
        log.info("No universes in the catalog — run `make db-seed` first.")
        return []

    def label(u):
        seeded = u["last_seeded_at"].strftime("%Y-%m-%d") if u["last_seeded_at"] else "never"
        nsym = len(u["symbols"]) if u["symbols"] else "ALL"
        return (f"{u['code']:<16} {u['category']:<7} {u['status']:<8} "
                f"seeded={seeded:<11} sym={str(nsym):<4} {u['description'] or ''}")

    choices = [questionary.Choice(label(u), value=u["code"], checked=u["enabled"])
               for u in universes]
    picked = questionary.checkbox(
        "Select instrument universes to seed (space toggles, enter confirms):",
        choices=choices).ask()
    if picked is None:                  # ctrl-c
        return []

    picked = set(picked)
    for u in universes:
        want = u["code"] in picked
        if want != u["enabled"]:
            set_universe_enabled(conn, u["code"], want)
            u["enabled"] = want
    enabled = [u for u in universes if u["enabled"]]
    if not enabled:
        print("No universes enabled — nothing to seed.")
    return enabled


# --- main -----------------------------------------------------------------

def parse_args():
    p = argparse.ArgumentParser(description="Seed instrument universes from Databento.")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--interactive", action="store_true", help="Pick universes to seed via a prompt")
    g.add_argument("--all-enabled", action="store_true", help="Seed every enabled universe")
    g.add_argument("--universe", help="Seed a single universe by code")
    p.add_argument("--dry-run", action="store_true", help="Estimate cost only; no fetch/write")
    p.add_argument("--max-cost", type=float, help="Abort if estimated cost exceeds this")
    return p.parse_args()


def main():
    args = parse_args()
    client = get_client()
    conn = get_ods_conn()
    try:
        confirm = False
        if args.universe:
            universes = load_universes(conn, code=args.universe)
            if not universes:
                raise SystemExit(f"no universe with code '{args.universe}'")
        elif args.interactive or (not args.all_enabled and sys.stdin.isatty()):
            universes = run_questionnaire(conn)
            confirm = True
        else:
            universes = load_universes(conn, enabled_only=True)
            if not universes:
                log.info("No enabled universes — run with --interactive to pick some.")
                return

        if universes:
            seed_universes(client, conn, universes, dry_run=args.dry_run,
                           max_cost=args.max_cost, persist_state=True, confirm=confirm)
    finally:
        conn.close()


if __name__ == "__main__":
    main()
