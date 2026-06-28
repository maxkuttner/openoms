CREATE TABLE order_state (
    order_id UUID PRIMARY KEY,
    client_order_id TEXT NOT NULL,
    principal_id UUID NOT NULL REFERENCES principal(id),
    portfolio_id UUID NOT NULL REFERENCES portfolio(id),
    account_id UUID NOT NULL REFERENCES account(id),
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
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
CREATE INDEX idx_order_state_connection ON order_state(broker_connection_code);
-- Exposure is keyed (portfolio, instrument) by the risk engine.
CREATE INDEX idx_order_state_portfolio_instrument ON order_state(portfolio_id, instrument_id);
-- Blotter / oversight: filter by submitter, sort by time.
CREATE INDEX idx_order_state_principal ON order_state(principal_id);
CREATE INDEX idx_order_state_created ON order_state(created_at);
