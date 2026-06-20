-- Derivative attributes: 1:1 extension so cash-equity rows stay clean instead
-- of carrying a dozen NULL derivative columns.
CREATE TABLE instrument_derivative (
    instrument_id     BIGINT PRIMARY KEY REFERENCES instrument(id) ON DELETE CASCADE,
    underlying_id     BIGINT REFERENCES instrument(id),  -- when the underlying is itself tracked
    underlying_symbol TEXT,                               -- fallback when it isn't (e.g. an index)
    option_kind       TEXT,                               -- CALL / PUT (options only)
    strike_price      NUMERIC,
    expiry_date       DATE,
    activation_date   DATE,
    CHECK (option_kind IS NULL OR option_kind IN ('CALL','PUT')),
    CHECK (underlying_id IS NOT NULL OR underlying_symbol IS NOT NULL)
);

CREATE INDEX idx_derivative_underlying ON instrument_derivative(underlying_id);
CREATE INDEX idx_derivative_expiry     ON instrument_derivative(expiry_date);

COMMENT ON TABLE instrument_derivative IS 'Master data — derivative-specific attributes (1:1 with instrument).';
