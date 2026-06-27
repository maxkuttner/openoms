#!/usr/bin/env python3
"""Seed the `venue` table from the ISO 10383 Market Identifier Code (MIC) registry.

mdm owns master data, so this connects as the cluster admin and `SET ROLE
mdm_master` to write `ods.public.venue`. Idempotent: upserts on `venue.code`
(= the MIC), so re-running just refreshes names/status.

Source: the official ISO 10383 MIC list, published by SWIFT (the registration
authority) at https://www.iso20022.org/market-identifier-codes — the URL lives on
iso20022.org because SWIFT registers both standards, but this is ISO 10383, not
ISO 20022. Download the CSV there, or let the script fetch the default URL.

Deps: Python 3.8+ stdlib only; shells out to `psql` (like the migrate runner).

Usage:
    python3 scripts/seed_venues.py                     # fetch default URL, upsert
    python3 scripts/seed_venues.py --source ISO10383_MIC.csv
    python3 scripts/seed_venues.py --dry-run           # preview, rolls back
    python3 scripts/seed_venues.py --sql-only          # print SQL; touch nothing
"""
import argparse
import csv
import io
import os
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

# Official ISO 10383 registry download (SWIFT is the registration authority).
# A local snapshot is also committed at data/ISO10383_MIC.csv — pass it via
# --source if this URL drifts (it has before).
DEFAULT_URL = "https://www.iso20022.org/sites/default/files/ISO10383_MIC/ISO10383_MIC.csv"


def load_env(path):
    env = {}
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def read_source(source):
    """Return the raw bytes of the MIC CSV from a URL or local path."""
    if source.startswith(("http://", "https://")):
        req = urllib.request.Request(source, headers={"User-Agent": "mdm-seed/1.0"})
        try:
            with urllib.request.urlopen(req) as resp:
                return resp.read()
        except urllib.error.HTTPError as e:
            sys.exit(
                f"error: could not download {source} ({e}).\n"
                "The ISO download URL changes periodically — grab the CSV from\n"
                "https://www.iso20022.org/market-identifier-codes and pass it with\n"
                "  --source /path/to/ISO10383_MIC.csv"
            )
    with open(source, "rb") as f:
        return f.read()


def decode(raw):
    for enc in ("utf-8-sig", "latin-1"):
        try:
            return raw.decode(enc)
        except UnicodeDecodeError:
            continue
    return raw.decode("utf-8", "replace")


def find_col(fieldnames, *needles, exact=None):
    """Locate a column tolerantly — header labels drift between yearly releases."""
    if exact:
        for fn in fieldnames:
            if fn.strip().upper() == exact:
                return fn
    for fn in fieldnames:
        up = fn.strip().upper()
        if all(n in up for n in needles):
            return fn
    return None


def reshape(text):
    """Map the wide MIC file down to the columns `venue` actually needs."""
    reader = csv.DictReader(io.StringIO(text))
    fns = reader.fieldnames or []
    c_mic = find_col(fns, exact="MIC")
    c_oprt = find_col(fns, "OPERATING", "MIC")        # parent/operating MIC
    c_name = find_col(fns, "MARKET", "NAME")
    c_country = find_col(fns, "COUNTRY")
    c_city = find_col(fns, exact="CITY") or find_col(fns, "CITY")
    c_status = find_col(fns, exact="STATUS") or find_col(fns, "STATUS")
    if not c_mic or not c_name:
        sys.exit(f"error: could not locate MIC/name columns in header: {fns}")

    rows, seen = [], set()
    for r in reader:
        code = (r.get(c_mic) or "").strip()
        if not code or code in seen:
            continue
        seen.add(code)
        status_raw = (r.get(c_status) or "").strip().upper()
        rows.append({
            "code": code,
            "name": (r.get(c_name) or "").strip() or code,
            "country": (r.get(c_country) or "").strip(),
            "city": (r.get(c_city) or "").strip(),
            "mic": (r.get(c_oprt) or "").strip(),     # operating/parent MIC
            "status": "INACTIVE" if status_raw in ("DELETED", "EXPIRED") else "ACTIVE",
        })
    return rows


def write_clean_csv(rows, path):
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=["code", "name", "country", "city", "mic", "status"])
        w.writeheader()
        w.writerows(rows)


# One transaction: load the clean CSV into a temp table, then upsert into venue.
UPSERT_SQL = """\
SET ROLE mdm_master;
SET search_path TO public;
CREATE TEMP TABLE venue_seed (code text, name text, country text, city text, mic text, status text) ON COMMIT DROP;
\\copy venue_seed FROM '{clean}' WITH (FORMAT csv, HEADER true)
INSERT INTO venue (code, name, country, city, mic, status)
SELECT code, name, NULLIF(country, ''), NULLIF(city, ''), NULLIF(mic, ''), status
FROM venue_seed
ON CONFLICT (code) DO UPDATE SET
    name = EXCLUDED.name,
    country = EXCLUDED.country,
    city = EXCLUDED.city,
    mic = EXCLUDED.mic,
    status = EXCLUDED.status,
    updated_at = now();
SELECT count(*) AS venues_total FROM venue;
"""


def build_sql(clean_path, dry_run):
    body = UPSERT_SQL.format(clean=clean_path)
    return "BEGIN;\n" + body + ("ROLLBACK;\n" if dry_run else "COMMIT;\n")


def psql_url(env, env_path):
    def need(k):
        v = env.get(k) or os.environ.get(k)
        if not v:
            sys.exit(f"error: {k} must be set (in {env_path} or the environment)")
        return v
    db = env.get("ODS_DB") or os.environ.get("ODS_DB") or "ods"
    return "postgres://{u}:{p}@{h}:{port}/{db}?sslmode=disable".format(
        u=need("ADMIN_USER"), p=need("ADMIN_PASSWORD"),
        h=need("DB_HOST"), port=need("DB_PORT"), db=db,
    )


def main():
    ap = argparse.ArgumentParser(description="Seed venue from the ISO 10383 MIC registry.")
    ap.add_argument("--source", default=DEFAULT_URL, help="MIC CSV URL or local path")
    ap.add_argument("--env", default=".env", help="env file (default: .env)")
    ap.add_argument("--dry-run", action="store_true", help="run inside a rolled-back transaction")
    ap.add_argument("--sql-only", action="store_true", help="print SQL + parsed sample; touch nothing")
    args = ap.parse_args()

    rows = reshape(decode(read_source(args.source)))
    print(f"parsed {len(rows)} venues from {args.source}", file=sys.stderr)

    clean_path = tempfile.NamedTemporaryFile("w", suffix=".csv", delete=False).name
    write_clean_csv(rows, clean_path)
    sql = build_sql(clean_path, args.dry_run)

    if args.sql_only:
        print(sql)
        print("--- parsed sample ---", file=sys.stderr)
        for r in rows[:5]:
            print(r, file=sys.stderr)
        return

    try:
        url = psql_url(load_env(args.env), args.env)
        proc = subprocess.run(
            ["psql", url, "-v", "ON_ERROR_STOP=1", "-f", "-"], input=sql, text=True
        )
        sys.exit(proc.returncode)
    finally:
        os.unlink(clean_path)


if __name__ == "__main__":
    main()
