ALTER TABLE oms_instrument        RENAME TO instrument;
ALTER TABLE oms_broker_instrument RENAME TO broker_instrument;

ALTER TABLE broker_instrument
    RENAME CONSTRAINT oms_broker_instrument_instrument_id_fkey
    TO broker_instrument_instrument_id_fkey;

ALTER INDEX idx_oms_instrument_status            RENAME TO idx_instrument_status;
ALTER INDEX idx_oms_instrument_symbol            RENAME TO idx_instrument_symbol;
ALTER INDEX idx_oms_broker_instrument_broker     RENAME TO idx_broker_instrument_broker;
ALTER INDEX idx_oms_broker_instrument_instrument RENAME TO idx_broker_instrument_instrument;

COMMENT ON TABLE instrument        IS 'Master data — shared across services.';
COMMENT ON TABLE broker_instrument IS 'Master data — shared across services.';
