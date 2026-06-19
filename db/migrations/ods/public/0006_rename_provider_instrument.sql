-- Rename the data-provider symbology table to mirror broker_instrument and make
-- it the symmetric bridge between the master surrogate key and a provider's own
-- identifiers. The table is empty today, so the rename is metadata-only.
--
-- Pair:
--   provider_instrument : master instrument_id <-> data provider (Databento, …)
--   broker_instrument   : master instrument_id <-> broker        (Alpaca, IBKR)

ALTER TABLE market_data_mapping RENAME TO provider_instrument;

ALTER INDEX idx_market_data_provider   RENAME TO idx_provider_instrument_provider;
ALTER INDEX idx_market_data_instrument RENAME TO idx_provider_instrument_instrument;

ALTER TABLE provider_instrument
    RENAME CONSTRAINT market_data_mapping_instrument_id_provider_code_key
                   TO provider_instrument_instrument_id_provider_code_key;

-- Provider's own instrument id, mirroring broker_instrument.native_id. Nullable.
-- Note: not every provider's native id is stable/unique over time (Databento's
-- publisher instrument_id rotates per dataset), so there is deliberately NO unique
-- constraint on it here — it's stored as a refreshed reference, not a key.
ALTER TABLE provider_instrument ADD COLUMN native_id TEXT;

-- The stable resolver key: a provider's symbol maps to exactly one master
-- instrument. Lets the table resolve provider symbol -> instrument_id once a 2nd
-- provider's symbols diverge from the master.
ALTER TABLE provider_instrument
    ADD CONSTRAINT provider_instrument_provider_symbol_uq UNIQUE (provider_code, provider_symbol);

COMMENT ON TABLE provider_instrument IS
    'Symbology bridge: master instrument_id <-> a data provider''s symbol (Databento, …). Mirror of broker_instrument.';
COMMENT ON COLUMN provider_instrument.native_id IS
    'Provider-native instrument id, refreshed each sync (e.g. Databento publisher instrument_id; may rotate).';
