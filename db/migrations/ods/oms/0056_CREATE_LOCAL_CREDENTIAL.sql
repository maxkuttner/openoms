-- Local (username-less, invite-created) accounts: an argon2 password hash, an
-- optional TOTP second factor and single-use recovery codes.
--
-- `totp_secret_enc` is encrypted with the master key, never stored in clear.
-- `totp_last_step` is the last accepted 30 s time step, so a code cannot be
-- replayed. `failed_attempts` / `locked_until` drive login throttling.
-- Recovery codes are stored only as hashes and marked used, never deleted.

CREATE TABLE local_credential (
    principal_id        UUID PRIMARY KEY REFERENCES principal(id),
    password_hash       TEXT NOT NULL,
    password_changed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    totp_secret_enc     BYTEA,
    totp_enrolled_at    TIMESTAMPTZ,
    totp_last_step      BIGINT,
    failed_attempts     INT NOT NULL DEFAULT 0,
    locked_until        TIMESTAMPTZ
);

CREATE TABLE recovery_code (
    principal_id UUID NOT NULL REFERENCES principal(id),
    code_hash    TEXT NOT NULL,
    used_at      TIMESTAMPTZ,
    PRIMARY KEY (principal_id, code_hash)
);
