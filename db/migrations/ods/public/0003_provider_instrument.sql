-- Symbology bridge between the master surrogate key and a data provider's own
-- identifiers — the symmetric sibling of broker_instrument.
--
-- Pair:
--   provider_instrument : master instrument_id <-> data provider (Databento, …)
--   broker_instrument   : master instrument_id <-> broker        (Alpaca, IBKR)

CREATE TABLE provider_instrument (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instrument_id    BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    provider_code    TEXT NOT NULL,
    provider_symbol  TEXT NOT NULL,
    provider_exchange TEXT,
    -- Provider-native instrument id, refreshed each sync (e.g. Databento publisher
    -- instrument_id; may rotate). Deliberately NOT unique — not every provider's
    -- native id is stable/unique over time, so it's a refreshed reference, not a key.
    native_id        TEXT,
    is_active        BOOLEAN NOT NULL DEFAULT true,
    metadata         JSONB,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, provider_code),
    -- The stable resolver key: a provider's symbol maps to exactly one master
    -- instrument, so the table can resolve provider symbol -> instrument_id.
    CONSTRAINT provider_instrument_provider_symbol_uq UNIQUE (provider_code, provider_symbol)
);

CREATE INDEX idx_provider_instrument_provider ON provider_instrument(provider_code);
CREATE INDEX idx_provider_instrument_instrument ON provider_instrument(instrument_id);

COMMENT ON TABLE provider_instrument IS
    'Symbology bridge: master instrument_id <-> a data provider''s symbol (Databento, …). Mirror of broker_instrument.';
COMMENT ON COLUMN provider_instrument.native_id IS
    'Provider-native instrument id, refreshed each sync (e.g. Databento publisher instrument_id; may rotate).';
