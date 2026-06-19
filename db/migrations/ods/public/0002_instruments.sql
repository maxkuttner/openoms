-- Master data (ODS / public schema): instruments + broker mappings.
-- Depends on reference data (venue, currency) created in 0001_reference.sql.

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

CREATE TABLE broker_instrument (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instrument_id   BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    broker_code     TEXT NOT NULL,
    broker_symbol   TEXT NOT NULL,
    broker_exchange TEXT,
    is_tradeable    BOOLEAN NOT NULL DEFAULT true,
    -- Broker-enforced order limits (vary per broker; the binding constraint at routing time).
    min_quantity    NUMERIC,
    max_quantity    NUMERIC,
    min_notional    NUMERIC,
    max_notional    NUMERIC,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, broker_code),
    CHECK (max_quantity IS NULL OR min_quantity IS NULL OR max_quantity >= min_quantity),
    CHECK (max_notional IS NULL OR min_notional IS NULL OR max_notional >= min_notional)
);

CREATE INDEX idx_instrument_status            ON instrument(status);
CREATE INDEX idx_instrument_isin              ON instrument(isin);
CREATE INDEX idx_instrument_venue             ON instrument(venue);
CREATE INDEX idx_instrument_class             ON instrument(asset_class, instrument_class);
CREATE INDEX idx_derivative_underlying        ON instrument_derivative(underlying_id);
CREATE INDEX idx_derivative_expiry            ON instrument_derivative(expiry_date);
CREATE INDEX idx_broker_instrument_broker     ON broker_instrument(broker_code);
CREATE INDEX idx_broker_instrument_instrument ON broker_instrument(instrument_id);

COMMENT ON TABLE instrument            IS 'Master data — instrument reference + trading microstructure.';
COMMENT ON TABLE instrument_derivative IS 'Master data — derivative-specific attributes (1:1 with instrument).';
COMMENT ON TABLE broker_instrument     IS 'Master data — broker symbol mappings.';
