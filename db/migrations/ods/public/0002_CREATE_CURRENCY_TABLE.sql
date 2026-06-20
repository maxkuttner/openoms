CREATE TABLE currency (
    code              TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    numeric_code      TEXT,
    minor_units       INT,
    is_active         BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE currency IS 'ISO 4217 currency codes and metadata';
