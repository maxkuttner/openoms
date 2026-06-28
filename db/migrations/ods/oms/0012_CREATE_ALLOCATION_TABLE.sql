-- Post-trade allocation (shaping): a block order's filled quantity distributed to
-- fund portfolios. Each row is one cost-preserving transfer from the block's staging
-- portfolio to a fund portfolio at the block's average fill price (no P&L realized on
-- the source — it's a conduit). Audit record + drives the position transfer.

CREATE TABLE allocation (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id          UUID NOT NULL REFERENCES order_state(order_id),
    from_portfolio_id UUID NOT NULL REFERENCES portfolio(id),   -- block / staging book
    to_portfolio_id   UUID NOT NULL REFERENCES portfolio(id),   -- the fund
    instrument_id     TEXT NOT NULL,
    qty               NUMERIC NOT NULL CHECK (qty > 0),
    price             NUMERIC NOT NULL,                         -- block avg fill price
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_allocation_order ON allocation(order_id);
