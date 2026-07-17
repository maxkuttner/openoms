-- Seed a minimal Binance Spot (testnet) crypto trading setup: a venue, the USDT
-- quote currency, a few spot pairs as instruments, their BROKER xref rows (order
-- routing + recon), and the broker_connection. Idempotent.
--
-- Crypto instrument model: symbol = the PAIR (BTCUSDT), currency = quote (USDT),
-- venue = BINANCE. The symbol names the pair because that is what the instrument
-- is -- symbol = base alone cannot represent BTC/USDT and BTC/BTC-quote variants
-- at once under UNIQUE (symbol, venue), and every venue calls it BTCUSDT anyway.
-- The xref carries that pair as external_symbol (order routing + market data) and
-- the base asset as external_native_id (BTC, what account balances report and
-- reconciliation matches on).
--
-- Account creation (principal/portfolio/account routing to binance-paper) is left
-- to the admin/cockpit — this seeds only the shared reference data.

-- Venue + quote currency.
INSERT INTO venue (code, name, country, status)
VALUES ('BINANCE', 'Binance', NULL, 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

INSERT INTO currency (code, name, minor_units, is_active)
VALUES ('USDT', 'Tether USD', 8, true)
ON CONFLICT (code) DO NOTHING;

-- Spot pairs (base asset vs USDT). Microstructure roughly matches Binance spot.
INSERT INTO instrument
    (symbol, venue, name, asset_class, instrument_class, currency, status,
     price_precision, size_precision, price_increment, size_increment)
VALUES
    ('BTCUSDT', 'BINANCE', 'Bitcoin',  'CRYPTO', 'SPOT', 'USDT', 'ACTIVE', 2, 5, 0.01,  0.00001),
    ('ETHUSDT', 'BINANCE', 'Ethereum', 'CRYPTO', 'SPOT', 'USDT', 'ACTIVE', 2, 4, 0.01,  0.0001),
    ('SOLUSDT', 'BINANCE', 'Solana',   'CRYPTO', 'SPOT', 'USDT', 'ACTIVE', 2, 3, 0.01,  0.001)
ON CONFLICT (symbol, venue) DO NOTHING;

-- BROKER xref: external_symbol = Binance pair (order routing);
-- external_native_id = base asset (recon match); is_tradeable + limits for routing.
INSERT INTO oms.instrument_xref
    (instrument_id, source_type, source_code, external_symbol, external_native_id,
     method, confidence, is_tradeable, min_quantity, min_notional)
SELECT i.id, 'BROKER', 'BINANCE', x.pair, x.base, 'manual', 'resolved', true, x.min_qty, 5
FROM (VALUES
    ('BTC', 'BTCUSDT', 0.00001),
    ('ETH', 'ETHUSDT', 0.0001),
    ('SOL', 'SOLUSDT', 0.001)
) AS x(base, pair, min_qty)
JOIN instrument i ON i.symbol = x.pair AND i.venue = 'BINANCE'
ON CONFLICT DO NOTHING;

-- PROVIDER xref: the same pair, but as a market-data identity rather than a
-- routing one. Without these rows the crypto is tradable but unmarkable — the
-- quote feed has no way to know Binance can price it, so /positions reports a
-- null mark for a position we can freely trade.
--
-- external_symbol is the pair as Binance's market-data streams report it (the `s`
-- field of bookTicker, uppercase); external_exchange is the venue, matching
-- instrument.venue. No native_id: the feed resolves by symbol, and Binance's
-- market data has no separate id worth storing.
INSERT INTO oms.instrument_xref
    (instrument_id, source_type, source_code, external_symbol, external_exchange,
     method, confidence)
SELECT i.id, 'PROVIDER', 'BINANCE', x.pair, 'BINANCE', 'manual', 'resolved'
FROM (VALUES
    ('BTC', 'BTCUSDT'),
    ('ETH', 'ETHUSDT'),
    ('SOL', 'SOLUSDT')
) AS x(base, pair)
JOIN instrument i ON i.symbol = x.pair AND i.venue = 'BINANCE'
ON CONFLICT DO NOTHING;

-- Routing target. Credentials resolved from env (BINANCE_PAPER_API_KEY/SECRET).
INSERT INTO oms.broker_connection (code, broker_code, environment, status)
VALUES ('binance-paper', 'BINANCE', 'PAPER', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;
