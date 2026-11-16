-- Append-only log of user/role/invite administration and sign-in events.
-- `actor` is the acting principal's code (or 'oms' for the system), as in
-- order_event.

CREATE TABLE audit_event (
    id                  BIGSERIAL PRIMARY KEY,
    at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor               TEXT NOT NULL,
    action              TEXT NOT NULL,
    target_principal_id UUID REFERENCES principal(id),
    detail              JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX idx_audit_event_at ON audit_event(at);
CREATE INDEX idx_audit_event_target ON audit_event(target_principal_id);
