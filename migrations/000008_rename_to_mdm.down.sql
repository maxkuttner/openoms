ALTER TABLE broker_instrument
    RENAME CONSTRAINT broker_instrument_instrument_id_fkey
    TO oms_broker_instrument_instrument_id_fkey;

ALTER INDEX idx_instrument_status            RENAME TO idx_oms_instrument_status;
ALTER INDEX idx_instrument_symbol            RENAME TO idx_oms_instrument_symbol;
ALTER INDEX idx_broker_instrument_broker     RENAME TO idx_oms_broker_instrument_broker;
ALTER INDEX idx_broker_instrument_instrument RENAME TO idx_oms_broker_instrument_instrument;

ALTER TABLE broker_instrument RENAME TO oms_broker_instrument;
ALTER TABLE instrument        RENAME TO oms_instrument;
