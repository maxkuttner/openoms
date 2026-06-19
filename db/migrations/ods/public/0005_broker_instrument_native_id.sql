-- Broker-native instrument id for order routing.
--
-- broker_symbol / broker_exchange are the human-facing mapping; the value we
-- actually route an order on is the broker's own instrument handle (Interactive
-- Brokers conId, Alpaca asset UUID, …). Store it explicitly so the order path is
-- instrument_id -> broker_instrument[broker] -> native_id -> broker API.
--
-- Nullable: populated by the per-broker symbology sync, so rows can exist before
-- the native id is resolved. Unique per broker so an inbound broker event (e.g. a
-- fill referencing the native id) resolves back to exactly one instrument; NULLs
-- are distinct in Postgres, so unsynced rows don't collide.

ALTER TABLE broker_instrument
    ADD COLUMN native_id TEXT;

ALTER TABLE broker_instrument
    ADD CONSTRAINT broker_instrument_broker_native_uq UNIQUE (broker_code, native_id);

COMMENT ON COLUMN broker_instrument.native_id
    IS 'Broker-native instrument id used for order routing (e.g. IBKR conId, Alpaca asset UUID).';
