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

CREATE INDEX idx_principal_status ON principal(status);
