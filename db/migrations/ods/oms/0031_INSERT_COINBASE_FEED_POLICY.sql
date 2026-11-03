-- The Coinbase ticker feed marks Coinbase's own products (venue COINBASE), which
-- no other feed prices, so it simply ranks first for them. Its candidates are
-- pinned to that venue, so it never competes with Binance or Bybit for theirs.
INSERT INTO provider_feed_policy (source_code, instrument_class, rank) VALUES
    ('COINBASE', 'SPOT', 10)
ON CONFLICT (source_code, instrument_class) DO NOTHING;
