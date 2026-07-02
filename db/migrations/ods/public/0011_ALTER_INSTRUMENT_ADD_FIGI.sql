-- FIGI / CUSIP anchors on the master (Bloomberg OpenFIGI symbology). Stamped by the
-- OMS symbology resolver (src/symbology_resolver.rs); the FIGI also lives on the
-- oms.instrument_xref bridge, which is the live cross-reference.

ALTER TABLE instrument ADD COLUMN figi  TEXT;
ALTER TABLE instrument ADD COLUMN cusip TEXT;

CREATE INDEX idx_instrument_figi ON instrument(figi);
