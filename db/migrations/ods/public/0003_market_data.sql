CREATE TABLE market_data_mapping (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instrument_id    BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    provider_code    TEXT NOT NULL,
    provider_symbol  TEXT NOT NULL,
    provider_exchange TEXT,
    is_active        BOOLEAN NOT NULL DEFAULT true,
    metadata         JSONB,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, provider_code)
);

CREATE INDEX idx_market_data_provider ON market_data_mapping(provider_code);
CREATE INDEX idx_market_data_instrument ON market_data_mapping(instrument_id);

COMMENT ON TABLE market_data_mapping IS 'Maps instruments to external data provider symbols (databento, Bloomberg, …).';
