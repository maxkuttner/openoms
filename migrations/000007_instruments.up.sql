CREATE TABLE oms_instrument (
    id          UUID PRIMARY KEY,
    code        TEXT NOT NULL UNIQUE,
    symbol      TEXT NOT NULL,
    asset_class TEXT NOT NULL,
    name        TEXT NOT NULL,
    currency    TEXT NOT NULL,
    exchange    TEXT,
    status      TEXT NOT NULL DEFAULT 'ACTIVE',
    metadata    JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (asset_class IN ('EQUITY', 'OPTION', 'FUTURE', 'FOREX', 'CRYPTO', 'FIXED_INCOME')),
    CHECK (status IN ('ACTIVE', 'INACTIVE', 'HALTED'))
);

CREATE TABLE oms_broker_instrument (
    id              UUID PRIMARY KEY,
    instrument_id   UUID NOT NULL REFERENCES oms_instrument(id),
    broker_code     TEXT NOT NULL,
    broker_symbol   TEXT NOT NULL,
    broker_exchange TEXT,
    is_tradeable    BOOLEAN NOT NULL DEFAULT true,
    metadata        JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, broker_code)
);

CREATE INDEX idx_oms_instrument_status ON oms_instrument(status);
CREATE INDEX idx_oms_instrument_symbol ON oms_instrument(symbol);
CREATE INDEX idx_oms_broker_instrument_broker ON oms_broker_instrument(broker_code);
CREATE INDEX idx_oms_broker_instrument_instrument ON oms_broker_instrument(instrument_id);
