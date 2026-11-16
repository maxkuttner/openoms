-- Which sign-in method an invite may be redeemed with: a local account, an
-- identity provider, or either.

ALTER TABLE invite
    ADD COLUMN method TEXT NOT NULL DEFAULT 'any'
        CHECK (method IN ('any', 'local', 'oidc'));
