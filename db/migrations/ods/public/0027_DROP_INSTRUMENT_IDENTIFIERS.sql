-- The OMS no longer enriches instruments with external identifiers. FIGI, CUSIP
-- and ISIN were only ever written by the OpenFIGI enricher and the symbology
-- resolver, both removed; whoever needs those identifiers maps them downstream.

DROP INDEX IF EXISTS idx_instrument_figi;
DROP INDEX IF EXISTS idx_instrument_isin;

ALTER TABLE instrument DROP COLUMN IF EXISTS figi;
ALTER TABLE instrument DROP COLUMN IF EXISTS cusip;
ALTER TABLE instrument DROP COLUMN IF EXISTS isin;
