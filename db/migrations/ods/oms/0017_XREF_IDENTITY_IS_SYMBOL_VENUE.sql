-- An external identity is Symbol@Venue per source — not Symbol@Venue@NativeId.
--
-- 0015 put external_native_id in the uniqueness key, contradicting its own column
-- comment ("may rotate ... a refreshed reference, not a key"). Databento's native
-- id rotates daily, so every seed run presented a "new" identity, never conflicted,
-- and INSERTed instead of UPDATEing: 53,653 rows for 27,252 option instruments,
-- growing unbounded per run (12,044 instruments had 3 rows — one per sync).
--
-- Verified before this change:
--   * no row is identified by native_id alone (external_symbol is never NULL/empty),
--   * only PROVIDER/DATABENTO has duplicates — every other source is already unique
--     on (source_type, source_code, external_symbol, external_exchange).
-- So dropping native_id from the key is a no-op for every source but Databento.
--
-- native_id remains a refreshed attribute. It is read only for BROKER rows, by
-- order routing (handlers.rs) and custodian recon (recon.rs), where the broker's id
-- is stable (e.g. Alpaca's asset UUID). No consumer reads a PROVIDER native_id —
-- Databento's live session ids come from SymbolMappingMsg, never from this table.

-- Collapse duplicate identities, keeping the most recently written row.
DELETE FROM instrument_xref x
USING instrument_xref keep
WHERE x.source_type = keep.source_type
  AND x.source_code = keep.source_code
  AND COALESCE(x.external_symbol, '')   = COALESCE(keep.external_symbol, '')
  AND COALESCE(x.external_exchange, '') = COALESCE(keep.external_exchange, '')
  AND x.id < keep.id;

DROP INDEX uq_instrument_xref_source;

-- Identity: one row per (source, symbol, venue). COALESCE so NULLs don't defeat it.
CREATE UNIQUE INDEX uq_instrument_xref_source ON instrument_xref (
    source_type, source_code,
    COALESCE(external_symbol, ''), COALESCE(external_exchange, '')
);
