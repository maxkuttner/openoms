-- openoms minimal seed: one tradeable instrument so a fresh no-creds install has
-- something in the catalog. Idempotent (re-runnable). Writes master tables in
-- `public` (owned by mdm_master), so this runs as the admin role via fixtures.sh.
--
-- Routing config (broker_connection) and the test identity chain are NOT here — they
-- live in `oms` and are created by the app at boot (see src/setup/bootstrap.rs:
-- ensure_broker_connections + ensure_dev_identity). Keeping master-data seeding
-- separate from oms-owned config is what lets each run under the right role.
--
-- Depends on `make db-seed` having loaded the FK targets (currency USD, venue ARCX).
-- instrument.id is GENERATED ALWAYS AS IDENTITY, so we never hardcode it: insert by
-- the (symbol, venue) natural key, then resolve the id for the mapping.

INSERT INTO instrument
    (symbol, venue, currency, asset_class, instrument_class, name,
     price_precision, size_precision, price_increment, size_increment, contract_size)
VALUES
    ('SPY', 'ARCX', 'USD', 'EQUITY', 'SPOT', 'SPDR S&P 500 ETF TRUST',
     2, 0, 0.01, 1, 1)
ON CONFLICT (symbol, venue) DO NOTHING;

-- Broker mapping (public.broker_instrument): Alpaca's view of SPY. native_id is
-- SPY's Alpaca asset id (a global, account-independent UUID), so the order path can
-- route on it.
INSERT INTO broker_instrument
    (instrument_id, broker_code, broker_symbol, broker_exchange, native_id, is_tradeable)
SELECT i.id, 'ALPACA', 'SPY', 'ARCA', 'b28f4066-5c6d-479b-a2af-85dc1a8f16fb', true
FROM   instrument i
WHERE  i.symbol = 'SPY' AND i.venue = 'ARCX'
ON CONFLICT (instrument_id, broker_code) DO NOTHING;

-- No feed mapping row: each feed derives what it can price from its own symbology
-- at startup. Note that nothing prices SPY *equity* today — the Databento feed
-- carries OPRA options only — so a position in it will be reported as unmarkable
-- by preflight.
