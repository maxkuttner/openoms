-- Entitlement: which principal may act on which portfolio. Account is no longer
-- part of the grant — a principal granted a portfolio trades it via the portfolio's
-- routed account(s). Account/connection-level routing permission is a separate,
-- later concern.

CREATE TABLE principal_portfolio_grant (
    id                  UUID PRIMARY KEY,
    principal_id        UUID NOT NULL REFERENCES principal(id),
    portfolio_id        UUID NOT NULL REFERENCES portfolio(id),
    can_trade           BOOLEAN NOT NULL DEFAULT false,
    can_view            BOOLEAN NOT NULL DEFAULT true,
    can_allocate        BOOLEAN NOT NULL DEFAULT false,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (principal_id, portfolio_id)
);

CREATE INDEX idx_grant_principal ON principal_portfolio_grant(principal_id);
CREATE INDEX idx_grant_portfolio ON principal_portfolio_grant(portfolio_id);
