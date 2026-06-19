CREATE TABLE order_state (
    order_id UUID PRIMARY KEY,
    client_order_id TEXT NOT NULL,
    book_id UUID NOT NULL,
    account_id UUID NOT NULL,
    instrument_id TEXT NOT NULL,
    side TEXT NOT NULL,
    order_type TEXT NOT NULL,
    time_in_force TEXT NOT NULL,
    limit_price NUMERIC,
    original_qty NUMERIC NOT NULL,
    leaves_qty NUMERIC NOT NULL,
    cum_qty NUMERIC NOT NULL,
    avg_px NUMERIC,
    status TEXT NOT NULL,
    resume_to_status TEXT,
    version BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_order_state_account_status ON order_state(account_id, status);
CREATE INDEX idx_order_state_instrument_status ON order_state(instrument_id, status);
