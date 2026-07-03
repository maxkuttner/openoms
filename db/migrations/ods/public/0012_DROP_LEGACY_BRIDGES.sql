-- Retire the legacy symbology bridges: superseded by the unified oms.instrument_xref
-- (routing + recon + seeders now read/write the xref). No inbound FKs reference these.
-- On a fresh setup they are created by 0005/0006 then dropped here — the seeders and
-- the SPY fixture populate the xref instead.

DROP TABLE IF EXISTS broker_instrument;
DROP TABLE IF EXISTS provider_instrument;
