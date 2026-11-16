-- An invite to sign in. Creating one creates the principal up front (status
-- INVITED); the first login whose verified email matches claims it. Only the
-- SHA-256 of the link token is stored.

CREATE TABLE invite (
    id            UUID PRIMARY KEY,
    token_hash    TEXT NOT NULL UNIQUE,
    email         TEXT NOT NULL,
    principal_id  UUID NOT NULL REFERENCES principal(id) ON DELETE CASCADE,
    created_by    TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    accepted_at   TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX idx_invite_principal ON invite(principal_id);
