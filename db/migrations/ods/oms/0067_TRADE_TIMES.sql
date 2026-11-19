-- Trade times: when a fill happened at the venue, kept apart from when the OMS
-- booked it. Booking time (order_event.created_at) is never overwritten and
-- event append order (global_position) is unchanged. Legacy rows stay NULL.

-- Venue execution time of the event; NULL for events without one.
-- ADD COLUMN is DDL, so the append-only row triggers do not fire.
ALTER TABLE order_event ADD COLUMN occurred_at TIMESTAMPTZ;

-- created_at was TIMESTAMP (no zone), written in UTC. COALESCE(occurred_at, created_at)
-- would mix zoned and zoneless values and mis-order under a non-UTC session, so make
-- it TIMESTAMPTZ. Existing values are read as the UTC instants they always were.
-- ALTER TYPE rewrites the table without firing the row triggers.
ALTER TABLE order_event
    ALTER COLUMN created_at TYPE TIMESTAMPTZ USING created_at AT TIME ZONE 'UTC';

ALTER TABLE order_state
    ADD COLUMN last_trade_at    TIMESTAMPTZ,  -- latest trade time among booked fills
    ADD COLUMN venue_created_at TIMESTAMPTZ,  -- when the venue created the order; set once
    ADD COLUMN venue_cum_qty    NUMERIC;      -- venue-reported cumulative qty; informational

-- Legacy filled orders: trade time falls back to booking time of the last fill event.
-- Position rows are left alone.
UPDATE order_state os SET last_trade_at = f.t
FROM (SELECT order_id, max(created_at) t FROM order_event
      WHERE event_type IN ('order_partially_filled','order_filled') GROUP BY order_id) f
WHERE os.order_id = f.order_id AND os.last_trade_at IS NULL;

-- The canonical-order key of the latest input applied to the position (fill,
-- base fee or transfer): (last_trade_at, last_trade_exec, last_trade_is_fee).
-- A newcomer that sorts before it rebuilds the position. NULL exec sorts last.
ALTER TABLE position
    ADD COLUMN last_trade_at     TIMESTAMPTZ,
    ADD COLUMN last_trade_exec   TEXT,
    ADD COLUMN last_trade_is_fee BOOLEAN;

-- Matches the blotter's default order (trade time, then booking time).
CREATE INDEX idx_order_state_last_trade_at ON order_state (last_trade_at DESC NULLS LAST, created_at DESC);
