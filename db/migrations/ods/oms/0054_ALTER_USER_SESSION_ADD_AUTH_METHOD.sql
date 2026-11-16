-- How a session was established, and when (if ever) its second factor passed.
-- Existing sessions came from the identity provider, hence the 'oidc' default.
-- `legacy_admin` marks sessions minted by the deprecated shared-secret admin
-- login. `mfa_verified_at` is set once a local session passes TOTP.

ALTER TABLE user_session
    ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'oidc'
        CHECK (auth_method IN ('oidc', 'local', 'legacy_admin')),
    ADD COLUMN mfa_verified_at TIMESTAMPTZ;
