-- Constituent symbols of an instrument_universe. A universe with no rows here is
-- seeded as the whole dataset (ALL_SYMBOLS); rows narrow it to just these
-- underlyings (e.g. index constituents).

CREATE TABLE instrument_universe_symbol (
    universe_code TEXT NOT NULL REFERENCES instrument_universe(code) ON DELETE CASCADE,
    symbol        TEXT NOT NULL,                         -- underlying/raw symbol
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (universe_code, symbol)
);

COMMENT ON TABLE instrument_universe_symbol IS
    'Constituent underlyings of an instrument_universe; empty set means the whole dataset.';
