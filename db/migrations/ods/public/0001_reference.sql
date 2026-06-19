CREATE TABLE venue (
    id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code      TEXT NOT NULL UNIQUE,             -- the MIC (ISO 10383)
    name      TEXT NOT NULL,
    country   TEXT,                             -- ISO 3166 country code
    city      TEXT,
    mic       TEXT,                             -- operating/parent MIC
    status    TEXT NOT NULL DEFAULT 'ACTIVE',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Note: timezone intentionally omitted — it isn't in the MIC registry and the
-- trading-hours timezone authority is `calendar.timezone`.

CREATE TABLE currency (
    code              TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    numeric_code      TEXT,
    minor_units       INT,
    is_active         BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE venue IS 'Trading venues (exchanges) — NYSE, NASDAQ, LSE, etc.';
COMMENT ON TABLE currency IS 'ISO 4217 currency codes and metadata';
