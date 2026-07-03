-- One-time: copy the legacy bridges into the unified oms.instrument_xref, before
-- the tables are dropped. Idempotent (ON CONFLICT). Run as ADMIN (reads public,
-- writes oms). Fresh setups don't need this — the seeders/fixture write xref directly.

-- Broker symbology + order-config -> BROKER rows.
INSERT INTO oms.instrument_xref
    (instrument_id, source_type, source_code, external_symbol, external_exchange, external_native_id,
     is_tradeable, min_quantity, max_quantity, min_notional, max_notional, method, confidence)
SELECT instrument_id, 'BROKER', broker_code, broker_symbol, broker_exchange, native_id,
       is_tradeable, min_quantity, max_quantity, min_notional, max_notional, 'broker_sync', 'resolved'
FROM public.broker_instrument
ON CONFLICT (source_type, source_code,
             COALESCE(external_native_id, ''), COALESCE(external_symbol, ''), COALESCE(external_exchange, ''))
DO UPDATE SET
    instrument_id = EXCLUDED.instrument_id,
    is_tradeable  = EXCLUDED.is_tradeable,
    min_quantity  = EXCLUDED.min_quantity,
    max_quantity  = EXCLUDED.max_quantity,
    min_notional  = EXCLUDED.min_notional,
    max_notional  = EXCLUDED.max_notional,
    updated_at    = now();

-- Data-provider symbology -> PROVIDER rows.
INSERT INTO oms.instrument_xref
    (instrument_id, source_type, source_code, external_symbol, external_exchange, external_native_id,
     method, confidence)
SELECT instrument_id, 'PROVIDER', provider_code, provider_symbol, provider_exchange, native_id,
       'seed_instruments', 'resolved'
FROM public.provider_instrument
ON CONFLICT (source_type, source_code,
             COALESCE(external_native_id, ''), COALESCE(external_symbol, ''), COALESCE(external_exchange, ''))
DO UPDATE SET
    instrument_id = EXCLUDED.instrument_id,
    updated_at    = now();
