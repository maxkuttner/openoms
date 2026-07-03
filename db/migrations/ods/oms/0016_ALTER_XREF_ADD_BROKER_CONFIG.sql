-- Broker order-config on the xref, so it fully replaces broker_instrument for
-- routing (which needs is_tradeable + the size/notional limits). NULL for
-- non-broker sources (PROVIDER / OPENFIGI).

ALTER TABLE instrument_xref ADD COLUMN is_tradeable  BOOLEAN;
ALTER TABLE instrument_xref ADD COLUMN min_quantity  NUMERIC;
ALTER TABLE instrument_xref ADD COLUMN max_quantity  NUMERIC;
ALTER TABLE instrument_xref ADD COLUMN min_notional  NUMERIC;
ALTER TABLE instrument_xref ADD COLUMN max_notional  NUMERIC;
