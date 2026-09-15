-- `actor` now holds the acting principal's `code`, not a fixed system label.
--
-- It was VARCHAR(30) when the only values it ever held were 'oms' and a broker
-- code. Human identity changed that: an authenticated command stamps `actor`
-- with `principal.code`, which is TEXT with no length limit — a JIT-provisioned
-- principal's code can fall back to the OIDC `sub`, commonly a 36-character
-- UUID. At 30 characters Postgres raises 22001 and the append fails; on the
-- routing path that append happens *after* the order is live at the broker, so
-- the truncation would leave a real order at the venue with no OrderRouted
-- event. Widen to TEXT to match the column it is copied from.
--
-- order_event is append-only, guarded by triggers on UPDATE and DELETE. ALTER
-- TABLE ... TYPE is DDL, not DML, so the row-level triggers do not fire; the
-- change is metadata-only (VARCHAR(n) → TEXT needs no rewrite or verification
-- scan) and existing rows are untouched.

ALTER TABLE order_event ALTER COLUMN actor TYPE TEXT;

COMMENT ON COLUMN order_event.actor IS
    'Who caused the event: the acting principal''s code for an authenticated command, ''oms'' for system-generated events, the broker''s name for broker-driven ones. TEXT, not VARCHAR(n) — principal.code is unbounded.';
