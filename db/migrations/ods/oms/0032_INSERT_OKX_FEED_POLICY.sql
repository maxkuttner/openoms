-- The OKX tickers feed marks OKX's own pairs (venue OKX), which no other feed
-- prices, so it simply ranks first for them. Its candidates are pinned to that
-- venue, so it never competes with Binance or Bybit for theirs.
INSERT INTO provider_feed_policy (source_code, instrument_class, rank) VALUES
    ('OKX', 'SPOT', 10)
ON CONFLICT (source_code, instrument_class) DO NOTHING;
