CREATE TABLE principal (
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

CREATE TABLE book (
    id                  UUID PRIMARY KEY,
    code                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    status              TEXT NOT NULL,
    base_currency       TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE', 'CLOSED'))
);

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

CREATE TABLE principal_book_account_grant (
    id                  UUID PRIMARY KEY,
    principal_id        UUID NOT NULL REFERENCES principal(id),
    book_id             UUID NOT NULL REFERENCES book(id),
    account_id          UUID NOT NULL REFERENCES account(id),
    can_trade           BOOLEAN NOT NULL DEFAULT false,
    can_view            BOOLEAN NOT NULL DEFAULT true,
    can_allocate        BOOLEAN NOT NULL DEFAULT false,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (principal_id, book_id, account_id)
);

CREATE INDEX idx_principal_status ON principal(status);
CREATE INDEX idx_book_status ON book(status);
CREATE INDEX idx_account_status ON account(status);

CREATE INDEX idx_grant_principal ON principal_book_account_grant(principal_id);
CREATE INDEX idx_grant_book ON principal_book_account_grant(book_id);
CREATE INDEX idx_grant_account ON principal_book_account_grant(account_id);

-- FK constraints on order_state (created in 0002, before the identity tables existed).
ALTER TABLE order_state
    ADD CONSTRAINT order_state_book_id_fkey FOREIGN KEY (book_id) REFERENCES book(id),
    ADD CONSTRAINT order_state_account_id_fkey FOREIGN KEY (account_id) REFERENCES account(id);
