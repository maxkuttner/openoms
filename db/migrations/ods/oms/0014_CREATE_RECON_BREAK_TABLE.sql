-- A single discrepancy found in a reconciliation run. instrument_id/symbol are
-- nullable to cover an unresolved custodian security (symbol only, no master id).

CREATE TABLE recon_break (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    recon_run_id   UUID NOT NULL REFERENCES recon_run(id) ON DELETE CASCADE,
    instrument_id  TEXT,                    -- master instrument id (as stored on position)
    symbol         TEXT,                    -- custodian symbol, when known
    oms_qty        NUMERIC NOT NULL DEFAULT 0,
    custodian_qty  NUMERIC NOT NULL DEFAULT 0,
    diff           NUMERIC NOT NULL DEFAULT 0,   -- oms_qty - custodian_qty
    kind           TEXT NOT NULL,                -- qty_mismatch | missing_in_custodian | missing_in_oms | unresolved_custodian_security
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_recon_break_run ON recon_break(recon_run_id);
