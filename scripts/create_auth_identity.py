#!/usr/bin/env python3
import os
import sys


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: create_auth_identity.py <provider_sub> <account_id> <portfolio_id>")
        return 1

    provider_sub, account_id, portfolio_id = sys.argv[1:4]
    database_url = os.environ.get("DATABASE_URL")
    if not database_url:
        print("DATABASE_URL must be set")
        return 1

    try:
        import psycopg2
    except ImportError:
        print("psycopg2 is required (pip install psycopg2-binary)")
        return 1

    conn = psycopg2.connect(database_url)
    try:
        with conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO auth_identity (
                        provider_sub,
                        account_id,
                        portfolio_id
                    ) VALUES (%s, %s, %s)
                    ON CONFLICT (provider_sub) DO UPDATE
                    SET account_id = EXCLUDED.account_id,
                        portfolio_id = EXCLUDED.portfolio_id,
                        updated_at = CURRENT_TIMESTAMP
                    """,
                    (provider_sub, account_id, portfolio_id),
                )
        print(f"auth_identity upserted for provider_sub={provider_sub}")
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    raise SystemExit(main())
