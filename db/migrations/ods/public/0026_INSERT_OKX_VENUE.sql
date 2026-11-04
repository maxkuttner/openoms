-- OKX spot pairs are seeded with venue 'OKX' (broker-first, like BINANCE, BYBIT and
-- COINBASE), so the venue must exist before the first universe load or every pair
-- fails the instrument.venue FK. Fresh installs get it from
-- db/scripts/seed_crypto_venues.sql too; this covers databases seeded earlier.
INSERT INTO venue (code, name, mic, status) VALUES ('OKX', 'OKX', NULL, 'ACTIVE')
ON CONFLICT (code) DO NOTHING;
