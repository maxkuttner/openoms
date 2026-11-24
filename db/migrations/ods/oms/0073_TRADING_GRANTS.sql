-- Trading grants: what a portfolio may trade. One row per grant; the kind's
-- parameters live in params (validated by the Rust `Grant` type).
CREATE TABLE trading_grant (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    subject_type text NOT NULL CHECK (subject_type IN ('portfolio')),
    subject_id   uuid NOT NULL,
    kind         text NOT NULL,
    params       jsonb NOT NULL,
    status       text NOT NULL DEFAULT 'ACTIVE' CHECK (status IN ('ACTIVE', 'REVOKED')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX trading_grant_subject_kind_params ON trading_grant (subject_type, subject_id, kind, params);

-- A venue grant owns a hidden account row per (portfolio, connection); several
-- portfolios may now sit on one connection and external account.
ALTER TABLE account DROP CONSTRAINT account_broker_connection_code_external_account_ref_key;
CREATE UNIQUE INDEX account_portfolio_connection ON account (portfolio_id, broker_connection_code) WHERE portfolio_id IS NOT NULL;
