-- Drop the (broker_code, native_id) uniqueness on broker_instrument. It held for
-- Alpaca (native_id = per-instrument asset UUID) but is wrong for Binance, where
-- native_id is the base asset and repeats across quote pairs (BTCUSDT and BTCUSDC
-- both base BTC). native_id is a routing/recon attribute, not a key; the real keys
-- are (instrument_id, broker_code) and (broker_code, broker_symbol, broker_exchange).

ALTER TABLE broker_instrument DROP CONSTRAINT IF EXISTS broker_instrument_broker_native_uq;
