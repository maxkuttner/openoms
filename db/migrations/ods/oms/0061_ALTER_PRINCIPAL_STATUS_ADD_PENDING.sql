-- Self-signup: an account a person created themselves waits as PENDING until
-- an admin approves it (ACTIVE) or rejects it (DISABLED). A PENDING account
-- can sign in, but sees nothing beyond who it is.
--
-- The account role shown in the UI (admin / user / pending) is derived, not
-- stored: admin = a principal_admin row, pending = this status, else user.
ALTER TABLE principal DROP CONSTRAINT principal_status_check;
ALTER TABLE principal ADD CONSTRAINT principal_status_check
    CHECK (status IN ('ACTIVE', 'INVITED', 'PENDING', 'DISABLED'));
