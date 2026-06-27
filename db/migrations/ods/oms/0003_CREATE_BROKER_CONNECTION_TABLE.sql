-- A configured broker connection: the routing/session target an order executes
-- through (broker + environment). Credentials live in env ({BROKER}_{ENV}_{KEY});
-- this row is the DB-side handle accounts reference. Split out of `account` so the
-- custodial account is separate from where it routes (cf. multi-broker routing).

CREATE TABLE broker_connection (
    code          TEXT PRIMARY KEY,                 -- 'alpaca-paper'
    broker_code   TEXT NOT NULL,                    -- ALPACA, IBKR
    environment   TEXT NOT NULL DEFAULT 'PAPER',
    status        TEXT NOT NULL DEFAULT 'ACTIVE',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (broker_code, environment),
    CHECK (environment IN ('PAPER','LIVE')),
    CHECK (status IN ('ACTIVE','DISABLED'))
);

COMMENT ON TABLE broker_connection IS
    'Configured broker+environment routing targets; creds resolved from env by (broker_code, environment).';
