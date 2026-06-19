CREATE TABLE calendar (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    code       TEXT NOT NULL UNIQUE,
    venue_id   UUID REFERENCES venue(id) ON DELETE SET NULL,
    timezone   TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE calendar_holiday (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    calendar_id UUID NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    date       DATE NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (calendar_id, date)
);

CREATE INDEX idx_calendar_holiday_date ON calendar_holiday(date);

COMMENT ON TABLE calendar IS 'Trading calendars per venue (defines trading hours and holidays)';
COMMENT ON TABLE calendar_holiday IS 'Market holidays — dates when the market is closed';
