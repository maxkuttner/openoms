CREATE TABLE auth_identity (
    provider_sub TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    portfolio_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_auth_identity_account ON auth_identity(account_id);
CREATE INDEX idx_auth_identity_portfolio ON auth_identity(portfolio_id);
