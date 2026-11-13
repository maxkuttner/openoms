-- A binding decision is an audit record: deleting its invite must not erase
-- it, and a decision is complete (decision, decided_at, decided_by together)
-- or absent.

ALTER TABLE pending_identity_binding
    DROP CONSTRAINT pending_identity_binding_invite_id_fkey,
    ADD CONSTRAINT pending_identity_binding_invite_id_fkey
        FOREIGN KEY (invite_id) REFERENCES invite(id) ON DELETE RESTRICT,
    ADD CONSTRAINT pending_identity_binding_decision_complete CHECK (
        (decision IS NULL) = (decided_at IS NULL) AND (decision IS NULL) = (decided_by IS NULL)
    );
