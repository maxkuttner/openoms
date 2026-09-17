-- Browser sessions for humans authenticated at the identity provider.
--
-- `token_hash` is SHA-256, deliberately unlike `api_key.secret_hash`, which is
-- bcrypt. bcrypt slows the guessing of low-entropy secrets and is affordable at
-- API-call rates; this value is 256 bits of randomness checked on every single
-- interaction, so the right cost is one indexed lookup.
--
-- Two clocks: `last_seen_at` drives a sliding idle timeout, `absolute_expires_at`
-- is the hard cap so a desk re-authenticates at least daily.

CREATE TABLE user_session (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    principal_id        UUID NOT NULL REFERENCES principal(id),
    token_hash          TEXT NOT NULL UNIQUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    absolute_expires_at TIMESTAMPTZ NOT NULL,
    revoked_at          TIMESTAMPTZ,
    user_agent          TEXT,
    ip                  INET
);

CREATE INDEX idx_user_session_live ON user_session (token_hash) WHERE revoked_at IS NULL;
CREATE INDEX idx_user_session_principal ON user_session (principal_id);
