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

CREATE INDEX idx_grant_principal ON principal_book_account_grant(principal_id);
CREATE INDEX idx_grant_book ON principal_book_account_grant(book_id);
CREATE INDEX idx_grant_account ON principal_book_account_grant(account_id);
