-- What a principal is watching for live price/day-change on the trade screen.
-- No surrogate id: the natural key is the pair itself, and there is nothing
-- else to reference it by. instrument_id is TEXT, not a real FK, matching
-- position's own instrument_id column — oms-schema tables don't take a
-- cross-schema FK into public.instrument.
CREATE TABLE watchlist_item (
    principal_id   UUID NOT NULL REFERENCES principal(id),
    instrument_id  TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (principal_id, instrument_id)
);
