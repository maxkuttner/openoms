-- A first OIDC sign-in that arrived with an invite token but is not yet trusted:
-- an admin approves or rejects binding (issuer, subject) to the invited person.
-- `untrusted_email` is what the provider claimed and is display-only; the
-- binding key is (issuer, subject), never the email.
--
-- At most one open (undecided) binding per identity; a decided one frees it.

CREATE TABLE pending_identity_binding (
    id              UUID PRIMARY KEY,
    invite_id       UUID NOT NULL REFERENCES invite(id) ON DELETE CASCADE,
    issuer          TEXT NOT NULL,
    subject         TEXT NOT NULL,
    untrusted_email TEXT,
    display_name    TEXT,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    decided_at      TIMESTAMPTZ,
    decided_by      TEXT,
    decision        TEXT CHECK (decision IN ('approved', 'rejected'))
);

CREATE UNIQUE INDEX uq_pending_identity_binding_open
    ON pending_identity_binding (issuer, subject) WHERE decision IS NULL;
CREATE INDEX idx_pending_identity_binding_invite ON pending_identity_binding (invite_id);
