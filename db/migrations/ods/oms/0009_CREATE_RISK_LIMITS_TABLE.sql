-- Pre-trade risk: per-(portfolio, instrument) limits and trading-state gate.
-- A missing row means "no limits configured" — orders pass risk unchecked.
-- The OMS submit path locks the row (SELECT ... FOR UPDATE) to serialize
-- concurrent submits within the same scope.

CREATE TABLE risk_limits (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    portfolio_id          UUID NOT NULL REFERENCES portfolio(id),
    instrument_id         TEXT NOT NULL,
    trading_state         TEXT NOT NULL DEFAULT 'ACTIVE',
    max_order_quantity    NUMERIC,
    max_order_notional    NUMERIC,
    max_position_quantity NUMERIC,
    max_position_notional NUMERIC,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (portfolio_id, instrument_id),
    CHECK (trading_state IN ('ACTIVE', 'REDUCING', 'HALTED')),
    CHECK (max_order_quantity    IS NULL OR max_order_quantity    > 0),
    CHECK (max_order_notional    IS NULL OR max_order_notional    > 0),
    CHECK (max_position_quantity IS NULL OR max_position_quantity > 0),
    CHECK (max_position_notional IS NULL OR max_position_notional > 0)
);
