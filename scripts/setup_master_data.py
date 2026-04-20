#!/usr/bin/env python3
"""
setup_master_data.py — bootstrap OMS master data for local development.

Creates:
  1. A principal (linked to your JWT sub)
  2. A trading book
  3. A broker account (ALPACA / PAPER by default)
  4. A can_trade grant linking principal → book → account  (inserted directly
     into the DB because there is no REST endpoint for grants yet)

Usage:
    python scripts/setup_master_data.py --token <jwt>

    # Override defaults:
    python scripts/setup_master_data.py \\
        --token <jwt> \\
        --base-url http://localhost:3001 \\
        --sub user_abc123 \\
        --broker-code ALPACA \\
        --environment PAPER \\
        --external-account-ref YOUR_ALPACA_ACCOUNT_ID

The script reads DATABASE_URL from .env (same directory as this script's
parent) to insert the grant directly.

Dependencies:
    pip install requests psycopg2-binary python-dotenv
"""

import argparse
import os
import sys
import uuid
from pathlib import Path

import requests

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def load_env(env_path: Path) -> dict:
    """Read key=value lines from a .env file, ignoring comments."""
    env = {}
    if not env_path.exists():
        return env
    for line in env_path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" in line:
            key, _, value = line.partition("=")
            # strip inline comments
            value = value.split("#")[0].strip()
            env[key.strip()] = value
    return env


def post(base_url: str, path: str, token: str | None, payload: dict) -> dict:
    url = f"{base_url}{path}"
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    resp = requests.post(url, json=payload, headers=headers)
    if not resp.ok:
        print(f"  ERROR {resp.status_code}: {resp.text}")
        sys.exit(1)
    return resp.json()


def post_no_content(base_url: str, path: str, token: str | None, payload: dict):
    url = f"{base_url}{path}"
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    resp = requests.post(url, json=payload, headers=headers)
    if not resp.ok:
        print(f"  ERROR {resp.status_code}: {resp.text}")
        sys.exit(1)


def insert_grant(database_url: str, principal_id: str, book_id: str, account_id: str):
    """Insert a can_trade grant directly into the DB (no REST endpoint exists)."""
    try:
        import psycopg2
    except ImportError:
        print()
        print("  psycopg2 not installed — insert the grant manually:")
        print(f"""
    INSERT INTO oms_principal_book_account_grant
        (id, principal_id, book_id, account_id, can_trade, can_view, can_allocate)
    VALUES
        ('{uuid.uuid4()}', '{principal_id}', '{book_id}', '{account_id}', true, true, false);
""")
        return

    conn = psycopg2.connect(database_url)
    try:
        with conn.cursor() as cur:
            grant_id = str(uuid.uuid4())
            cur.execute(
                """
                INSERT INTO oms_principal_book_account_grant
                    (id, principal_id, book_id, account_id, can_trade, can_view, can_allocate)
                VALUES (%s, %s, %s, %s, true, true, false)
                ON CONFLICT (principal_id, book_id, account_id) DO NOTHING
                """,
                (grant_id, principal_id, book_id, account_id),
            )
        conn.commit()
        print(f"  grant inserted  id={grant_id}")
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Bootstrap OMS master data")
    parser.add_argument("--token", default=None, help="JWT with org_role=org:admin (required when OMS JWT verification is enabled)")
    parser.add_argument("--base-url", default="http://localhost:3001")
    parser.add_argument(
        "--sub",
        default=None,
        help="JWT subject (external_subject for principal). Decoded from token if omitted.",
    )
    parser.add_argument("--broker-code", default="ALPACA")
    parser.add_argument("--environment", default="PAPER", choices=["PAPER", "LIVE"])
    parser.add_argument(
        "--external-account-ref",
        default="paper-default",
        help="Your broker account ID (e.g. Alpaca account number)",
    )
    parser.add_argument("--key-id", default=None, dest="key_id", help="API key ID (ak_...) to register against the principal")
    parser.add_argument("--secret", default=None, help="API secret (sk_...) to register against the principal")
    args = parser.parse_args()

    # Load .env from project root (one level up from scripts/)
    env_file = Path(__file__).parent.parent / ".env"
    env = load_env(env_file)
    database_url = os.environ.get("DATABASE_URL") or env.get("DATABASE_URL")
    if not database_url:
        print("WARNING: DATABASE_URL not found — grant will not be inserted automatically")

    # Decode sub from token if not provided explicitly
    sub = args.sub
    if not sub and args.token:
        try:
            import base64, json as _json
            payload_b64 = args.token.split(".")[1]
            # pad to multiple of 4
            payload_b64 += "=" * (-len(payload_b64) % 4)
            claims = _json.loads(base64.urlsafe_b64decode(payload_b64))
            sub = claims.get("sub", "unknown")
            print(f"Decoded sub from token: {sub}")
        except Exception:
            sub = "unknown"
            print(f"Could not decode sub from token, using: {sub}")
    if not sub:
        sub = "unknown"

    base = args.base_url

    # 1. Create principal
    print("\n[1] Creating principal...")
    principal = post(base, "/admin/principals", args.token, {
        "code": f"principal-{sub[:16]}",
        "principal_type": "HUMAN",
        "external_subject": sub,
        "display_name": f"Dev user ({sub[:12]})",
        "status": "ACTIVE",
    })
    print(f"  principal created  id={principal['id']}  code={principal['code']}")

    # 2. Create book
    print("\n[2] Creating book...")
    book = post(base, "/admin/books", args.token, {
        "code": "dev-book-1",
        "name": "Development Book",
        "status": "ACTIVE",
        "base_currency": "USD",
    })
    print(f"  book created       id={book['id']}  code={book['code']}")

    # 3. Create account
    print("\n[3] Creating account...")
    account = post(base, "/admin/accounts", args.token, {
        "code": f"{args.broker_code.lower()}-{args.environment.lower()}-1",
        "broker_code": args.broker_code,
        "environment": args.environment,
        "external_account_ref": args.external_account_ref,
        "status": "ACTIVE",
    })
    print(f"  account created    id={account['id']}  code={account['code']}")

    # 4. Grant
    print("\n[4] Inserting can_trade grant...")
    if database_url:
        insert_grant(database_url, principal["id"], book["id"], account["id"])
    else:
        insert_grant(None, principal["id"], book["id"], account["id"])

    # 5. Register API key (optional)
    if args.key_id and args.secret:
        print("\n[5] Registering API key...")
        post_no_content(base, f"/admin/principals/{principal['id']}/keys", args.token, {
            "key_id": args.key_id,
            "secret": args.secret,
        })
        print(f"  key registered  key_id={args.key_id}")

    # Summary
    print("\n" + "=" * 60)
    print("Master data ready. Use these with submit_order.py:")
    print(f"  PRINCIPAL_ID = \"{principal['id']}\"")
    print(f"  BOOK_ID      = \"{book['id']}\"")
    print(f"  ACCOUNT_ID   = \"{account['id']}\"")
    if args.key_id:
        print(f"  KEY_ID       = \"{args.key_id}\"")
    print("=" * 60)


if __name__ == "__main__":
    main()
