-- A password the host CLI generated (`oms auth bootstrap-admin` /
-- `recover-admin`) is someone else's choice: the session it opens may only
-- change it. If it is never used to sign in, it stops working after 24 h.
ALTER TABLE local_credential
    ADD COLUMN must_change_password BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN password_expires_at TIMESTAMPTZ;
