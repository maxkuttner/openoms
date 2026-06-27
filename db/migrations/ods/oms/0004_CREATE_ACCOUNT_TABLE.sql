-- A custodial account at a broker. Routing coordinates (broker_code, environment)
-- live on broker_connection; this table holds the account identity + the broker's
-- own account ref the order trades in.

CREATE TABLE account (
    id                   UUID PRIMARY KEY,
    code                 TEXT NOT NULL UNIQUE,
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    external_account_ref TEXT NOT NULL,
    status               TEXT NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (broker_connection_code, external_account_ref),
    CHECK (status IN ('ACTIVE', 'SUSPENDED', 'CLOSED'))
);

CREATE INDEX idx_account_status ON account(status);
CREATE INDEX idx_account_broker_connection ON account(broker_connection_code);
