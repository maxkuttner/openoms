-- Invites are gone: accounts come from the first signup, self-signup, an
-- admin's "Add user", or SSO signup/merge (see src/accounts.rs). Nothing
-- reads or writes these tables any more. INVITED stays in
-- principal_status_check for rows that may still carry it; an admin can
-- disable those from the Users page.
DROP TABLE pending_identity_binding;
DROP TABLE invite;
