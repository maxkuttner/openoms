-- Crypto exchange venues. These are NOT in the ISO 10383 MIC registry (seed_venues.py
-- only loads real MICs), so they must be seeded explicitly. Synthetic codes, mic NULL.
--
-- Broker-first seeding sets a crypto instrument's venue to the exchange code
-- (e.g. Binance pairs -> venue 'BINANCE'), so this venue must exist before
-- `oms setup sync-broker --broker binance` runs, or every pair fails the venue FK.
-- BYBIT is seeded too so a future Bybit broker (or venue-attributed feed) has a home.
-- Run as mdm_master (owner of public), matching seed_currencies.sql.

SET ROLE mdm_master;
SET search_path TO public;

INSERT INTO venue (code, name, mic, status) VALUES
    ('BINANCE', 'Binance', NULL, 'ACTIVE'),
    ('BYBIT',   'Bybit',   NULL, 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

RESET ROLE;
