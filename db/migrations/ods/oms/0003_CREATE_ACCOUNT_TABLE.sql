CREATE TABLE account (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    broker_code         TEXT NOT NULL,
    external_account_ref TEXT NOT NULL,
    environment         TEXT NOT NULL DEFAULT 'PAPER',
    status              TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (broker_code, environment, external_account_ref),
    CHECK (environment IN ('PAPER', 'LIVE')),
    CHECK (status IN ('ACTIVE', 'SUSPENDED', 'CLOSED'))
);

CREATE INDEX idx_account_status ON account(status);
