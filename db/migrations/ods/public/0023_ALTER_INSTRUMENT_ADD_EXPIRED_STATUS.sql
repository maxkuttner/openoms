-- Allow an EXPIRED status: a dated contract whose expiry instant has passed. Set by
-- the OMS expiry sweep (src/expiry.rs), never by hand.
--
-- Distinct from INACTIVE on purpose. INACTIVE is a judgement about a listing
-- (delisted, withdrawn); EXPIRED is a scheduled fact that was knowable from
-- `expires_at` the day the contract was created. Both are excluded by the
-- `status = 'ACTIVE'` filters on the order path and the quote-subscription path, so
-- the sweep needs no other code to take effect — but only one of them says why.
--
-- The original CHECK was anonymous (0003_CREATE_INSTRUMENT_TABLE.sql), so Postgres
-- named it instrument_status_check.

ALTER TABLE instrument DROP CONSTRAINT IF EXISTS instrument_status_check;
ALTER TABLE instrument ADD CONSTRAINT instrument_status_check
    CHECK (status IN ('ACTIVE', 'INACTIVE', 'HALTED', 'EXPIRED'));
