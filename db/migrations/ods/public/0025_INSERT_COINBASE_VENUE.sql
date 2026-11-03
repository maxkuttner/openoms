-- Coinbase spot products are seeded with venue 'COINBASE' (broker-first, like
-- BINANCE and BYBIT), so the venue must exist before the first universe load or
-- every product fails the instrument.venue FK. Fresh installs get it from
-- db/scripts/seed_crypto_venues.sql too; this covers databases seeded earlier.
INSERT INTO venue (code, name, mic, status) VALUES ('COINBASE', 'Coinbase', NULL, 'ACTIVE')
ON CONFLICT (code) DO NOTHING;
