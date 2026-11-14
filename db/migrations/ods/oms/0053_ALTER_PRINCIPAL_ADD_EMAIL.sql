-- A person's email: set when an invite is created (stored lowercased) and read
-- back when an admin issues a link invite for a second sign-in provider. It is
-- never used to resolve a login; only (issuer, subject) is.
--
-- Unique among principals that are not DISABLED, so a revoked invitee's address
-- can be invited again.

ALTER TABLE principal ADD COLUMN email TEXT;

ALTER TABLE principal ADD CONSTRAINT principal_email_lowercase
    CHECK (email IS NULL OR email = lower(email));

CREATE UNIQUE INDEX uq_principal_email_live ON principal(email)
    WHERE email IS NOT NULL AND status <> 'DISABLED';
