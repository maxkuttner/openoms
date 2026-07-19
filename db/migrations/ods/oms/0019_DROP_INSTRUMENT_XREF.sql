-- Retire the unified instrument_xref. Its two responsibilities are now split into
-- purpose-built master tables: broker routing → public.broker_instrument (0016),
-- feed market-data mapping → public.feed_instrument (0017). Order routing, recon,
-- the quote feed, and the resolver all read the new tables; nothing reads the xref.
--
-- provider_feed_policy stays (ranked failover across feeds); its source_code column
-- already holds feed codes (DATABENTO/BINANCE/BYBIT).

DROP TABLE IF EXISTS instrument_xref;
