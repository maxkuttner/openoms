CREATE TABLE api_key (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    principal_id UUID NOT NULL REFERENCES principal(id),
    key_id       TEXT NOT NULL UNIQUE,
    secret_hash  TEXT NOT NULL,
    name         TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at   TIMESTAMPTZ
);

CREATE INDEX ON api_key (key_id) WHERE revoked_at IS NULL;
