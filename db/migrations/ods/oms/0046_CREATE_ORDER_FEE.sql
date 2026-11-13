-- Venue fees booked against an order: one row per venue-reported fee, keyed by
-- the venue's fee id so a replayed report books once. Fees stay in their own
-- currency (never FX-converted). The position effect is replayed from the
-- order_fee_charged events; this table is the source of truth for fee rows.
-- Runtime-role grants come from db/access/ods.sql (GRANT ON ALL TABLES).

CREATE TABLE order_fee (
    order_id      UUID NOT NULL REFERENCES order_state(order_id),
    fee_id        TEXT NOT NULL,
    execution_id  TEXT,
    portfolio_id  UUID NOT NULL,
    instrument_id TEXT NOT NULL,
    amount        NUMERIC NOT NULL,
    currency      TEXT NOT NULL,
    effect        TEXT NOT NULL CHECK (effect IN ('quote','base','other')),
    booked_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (order_id, fee_id)
);
CREATE INDEX order_fee_position_idx ON order_fee (portfolio_id, instrument_id, currency);
