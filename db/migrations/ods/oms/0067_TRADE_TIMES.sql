-- Trade times: when a fill happened at the venue, kept apart from when the OMS
-- booked it. Booking time (order_event.created_at) is never overwritten and
-- event append order (global_position) is unchanged. Legacy rows stay NULL.

-- Venue execution time of the event; NULL for events without one.
-- ADD COLUMN is DDL, so the append-only row triggers do not fire.
ALTER TABLE order_event ADD COLUMN occurred_at TIMESTAMPTZ;

ALTER TABLE order_state
    ADD COLUMN last_trade_at    TIMESTAMPTZ,  -- latest trade time among booked fills
    ADD COLUMN venue_created_at TIMESTAMPTZ,  -- when the venue created the order; set once
    ADD COLUMN venue_cum_qty    NUMERIC;      -- venue-reported cumulative qty; informational

ALTER TABLE position ADD COLUMN last_trade_at TIMESTAMPTZ;

CREATE INDEX idx_order_state_last_trade_at ON order_state (last_trade_at DESC NULLS LAST);
