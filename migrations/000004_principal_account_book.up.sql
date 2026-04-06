CREATE TABLE oms_principal (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    principal_type      TEXT NOT NULL,
    external_subject    TEXT UNIQUE,
    display_name        TEXT,
    status              TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (principal_type IN ('HUMAN', 'SERVICE', 'STRATEGY', 'DESK')),
    CHECK (status IN ('ACTIVE', 'DISABLED'))
);

CREATE TABLE oms_book (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    status              TEXT NOT NULL,
    base_currency       TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE', 'CLOSED'))
);

CREATE TABLE oms_account (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    broker_code         TEXT NOT NULL,
    external_account_ref TEXT NOT NULL,
    status              TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (broker_code, external_account_ref),
    CHECK (status IN ('ACTIVE', 'SUSPENDED', 'CLOSED'))
);

CREATE TABLE oms_principal_book_account_grant (
    id                  UUID PRIMARY KEY,
    principal_id        UUID NOT NULL REFERENCES oms_principal(id),
    book_id             UUID NOT NULL REFERENCES oms_book(id),
    account_id          UUID NOT NULL REFERENCES oms_account(id),
    can_trade           BOOLEAN NOT NULL DEFAULT false,
    can_view            BOOLEAN NOT NULL DEFAULT true,
    can_allocate        BOOLEAN NOT NULL DEFAULT false,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (principal_id, book_id, account_id)
);

CREATE INDEX idx_oms_principal_status ON oms_principal(status);
CREATE INDEX idx_oms_book_status ON oms_book(status);
CREATE INDEX idx_oms_account_status ON oms_account(status);

CREATE INDEX idx_oms_grant_principal ON oms_principal_book_account_grant(principal_id);
CREATE INDEX idx_oms_grant_book ON oms_principal_book_account_grant(book_id);
CREATE INDEX idx_oms_grant_account ON oms_principal_book_account_grant(account_id);
