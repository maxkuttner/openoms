-- When the OMS began following a co-located strategy's account: gap recon
-- never books trades from before it.

ALTER TABLE strategy ADD COLUMN follow_since TIMESTAMPTZ;
