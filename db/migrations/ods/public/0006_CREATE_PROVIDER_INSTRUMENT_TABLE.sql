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
    -- The provider's own exchange label (its raw string, e.g. Databento's exchange
    -- field), NOT necessarily our canonical venue MIC. NOT NULL so it can anchor the
    -- resolver key below (NULLs are distinct in Postgres and would defeat it).
    provider_exchange TEXT NOT NULL,
    -- Provider-native instrument id, refreshed each sync (e.g. Databento publisher
    -- instrument_id; may rotate). Deliberately NOT unique — not every provider's
    -- native id is stable/unique over time, so it's a refreshed reference, not a key.
    native_id        TEXT,
    is_active        BOOLEAN NOT NULL DEFAULT true,
    metadata         JSONB,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, provider_code),
    -- The stable resolver key. A provider symbol resolves to one master instrument
    -- per exchange — a consolidated provider lists the same symbol across venues
    -- (SPY on ARCX, XNAS, …), each a distinct (symbol, venue) master instrument,
    -- so the exchange is part of the key. Mirrors Nautilus's Symbol@Venue identity.
    CONSTRAINT provider_instrument_provider_symbol_uq
        UNIQUE (provider_code, provider_symbol, provider_exchange)
);

CREATE INDEX idx_provider_instrument_provider ON provider_instrument(provider_code);
CREATE INDEX idx_provider_instrument_instrument ON provider_instrument(instrument_id);

COMMENT ON TABLE provider_instrument IS
    'Symbology bridge: master instrument_id <-> a data provider''s symbol (Databento, …). Mirror of broker_instrument.';
COMMENT ON COLUMN provider_instrument.native_id IS
    'Provider-native instrument id, refreshed each sync (e.g. Databento publisher instrument_id; may rotate).';
