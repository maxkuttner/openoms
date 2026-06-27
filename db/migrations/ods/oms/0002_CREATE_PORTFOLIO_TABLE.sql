CREATE TABLE portfolio (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    status              TEXT NOT NULL,
    base_currency       TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE', 'CLOSED'))
);

CREATE INDEX idx_portfolio_status ON portfolio(status);
