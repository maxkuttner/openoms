-- Master data (ODS / public schema): the instrument reference table.
-- Depends on reference data (venue, currency).

CREATE TABLE instrument (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,

    symbol           TEXT NOT NULL,                 -- venue-native ticker
    venue       TEXT NOT NULL REFERENCES venue(code),
    isin             TEXT,                          -- ISO 6166 security id (shared across listings)
    name             TEXT NOT NULL,

    asset_class      TEXT NOT NULL,                 -- what it gives exposure to
    instrument_class TEXT NOT NULL,                 -- what form it trades as
    -- Convenience flag derived from instrument_class (single source of truth; can't drift).
    is_derivative    BOOLEAN GENERATED ALWAYS AS
                     (instrument_class IN ('FUTURE','FORWARD','OPTION','SWAP','CFD','WARRANT')) STORED,

    currency         TEXT NOT NULL REFERENCES currency(code),
    status           TEXT NOT NULL DEFAULT 'ACTIVE',

    -- Trading microstructure
    price_precision  INTEGER NOT NULL,              -- decimal places for price
    size_precision   INTEGER NOT NULL DEFAULT 0,    -- decimal places for quantity
    price_increment  NUMERIC NOT NULL,              -- tick size
    size_increment   NUMERIC NOT NULL DEFAULT 1,    -- quantity step
    lot_size         NUMERIC,                       -- min tradeable block (nullable)
    contract_size    NUMERIC NOT NULL DEFAULT 1,    -- units of underlying per contract (1 for cash)

    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (symbol, venue),                         -- listing-level natural key
    CHECK (asset_class      IN ('EQUITY','FX','COMMODITY','DEBT','INDEX','CRYPTO','ALTERNATIVE')),
    CHECK (instrument_class IN ('SPOT','FUTURE','FORWARD','OPTION','SWAP','CFD','BOND','WARRANT')),
    CHECK (status           IN ('ACTIVE','INACTIVE','HALTED')),
    CHECK (price_precision >= 0 AND size_precision >= 0),
    CHECK (price_increment > 0 AND size_increment > 0 AND contract_size > 0)
);

CREATE INDEX idx_instrument_status ON instrument(status);
CREATE INDEX idx_instrument_isin   ON instrument(isin);
CREATE INDEX idx_instrument_venue  ON instrument(venue);
CREATE INDEX idx_instrument_class  ON instrument(asset_class, instrument_class);

COMMENT ON TABLE instrument IS 'Master data — instrument reference + trading microstructure.';
