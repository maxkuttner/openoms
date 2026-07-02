-- Unified instrument cross-reference: any external identifier (broker / provider /
-- custodian / OpenFIGI) -> master instrument, anchored on FIGI. The consolidation
-- target that generalises public.broker_instrument + public.provider_instrument.
--
-- Lives in the `oms` schema (owned by oms_user) so the OMS resolver writes it at
-- runtime. instrument_id references public.instrument(id) but carries no FK — the
-- runtime role has SELECT, not REFERENCES, on the public catalog; the resolver only
-- ever inserts ids it just read from public.instrument.

CREATE TABLE instrument_xref (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instrument_id      BIGINT,             -- master public.instrument(id); NULL = identified, no master
    source_type        TEXT NOT NULL,      -- BROKER | PROVIDER | CUSTODIAN | OPENFIGI | MANUAL
    source_code        TEXT NOT NULL,      -- ALPACA | DATABENTO | OPENFIGI | ...
    external_symbol    TEXT,
    external_exchange  TEXT,
    external_native_id TEXT,
    figi               TEXT,               -- FIGI anchor
    method             TEXT NOT NULL,      -- openfigi | symbol_venue | manual
    confidence         TEXT,               -- resolved | ambiguous
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per (source, external identity); COALESCE so NULLs don't defeat the dedup.
CREATE UNIQUE INDEX uq_instrument_xref_source ON instrument_xref (
    source_type, source_code,
    COALESCE(external_native_id, ''), COALESCE(external_symbol, ''), COALESCE(external_exchange, '')
);
CREATE INDEX idx_instrument_xref_instrument ON instrument_xref(instrument_id);
CREATE INDEX idx_instrument_xref_figi ON instrument_xref(figi);
CREATE INDEX idx_instrument_xref_lookup ON instrument_xref(source_code, external_symbol);
