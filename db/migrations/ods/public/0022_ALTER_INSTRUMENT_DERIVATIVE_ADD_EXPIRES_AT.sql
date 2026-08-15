-- The UTC instant a contract stops trading, derived from `expiry_date` plus the
-- venue calendar's timezone + close_time. Written by the OMS expiry job
-- (src/expiry.rs), which also re-derives it if a calendar is corrected.
--
-- `expiry_date` stays: it is the OSI truth and the venue-local calendar date that
-- symbology and broker reconciliation speak in. This column is the derived instant
-- the sweep compares against now(), so the comparison never depends on the DB's
-- timezone setting.

ALTER TABLE instrument_derivative ADD COLUMN expires_at TIMESTAMPTZ;

CREATE INDEX idx_derivative_expires_at ON instrument_derivative(expires_at);
