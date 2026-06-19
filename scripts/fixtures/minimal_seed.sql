-- openoms minimal seed: one tradeable instrument so a fresh install can place a
-- paper order without any data-provider creds. Idempotent (re-runnable).
--
-- Depends on `make db-seed` having loaded the FK targets (currency USD, venue ARCX).
-- instrument.id is GENERATED ALWAYS AS IDENTITY, so we never hardcode it: insert by
-- the (symbol, venue) natural key, then resolve the id for the mappings.

INSERT INTO instrument
    (symbol, venue, currency, asset_class, instrument_class, name,
     price_precision, size_precision, price_increment, size_increment, contract_size)
VALUES
    ('SPY', 'ARCX', 'USD', 'EQUITY', 'SPOT', 'SPDR S&P 500 ETF TRUST',
     2, 0, 0.01, 1, 1)
ON CONFLICT (symbol, venue) DO NOTHING;

-- Databento's view of it.
INSERT INTO provider_instrument
    (instrument_id, provider_code, provider_symbol, provider_exchange)
SELECT id, 'DATABENTO', 'SPY', 'ARCX'
FROM   instrument WHERE symbol = 'SPY' AND venue = 'ARCX'
ON CONFLICT (instrument_id, provider_code) DO NOTHING;

-- Alpaca's view of it. native_id is SPY's Alpaca asset id (a global, account-
-- independent UUID), so the order path can route on it.
INSERT INTO broker_instrument
    (instrument_id, broker_code, broker_symbol, broker_exchange, native_id, is_tradeable)
SELECT id, 'ALPACA', 'SPY', 'ARCA', 'b28f4066-5c6d-479b-a2af-85dc1a8f16fb', true
FROM   instrument WHERE symbol = 'SPY' AND venue = 'ARCX'
ON CONFLICT (instrument_id, broker_code) DO NOTHING;
