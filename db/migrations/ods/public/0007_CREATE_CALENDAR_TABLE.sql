CREATE TABLE calendar (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code       TEXT NOT NULL UNIQUE,
    venue_id   UUID REFERENCES venue(id) ON DELETE SET NULL,
    timezone   TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

COMMENT ON TABLE calendar IS 'Trading calendars per venue (defines trading hours and holidays)';
