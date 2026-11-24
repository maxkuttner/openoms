-- FIGI enrichment and the symbology backfill are gone, so only one job kind
-- writes a connection's universe again. Drop their history and return to a
-- single "one active job per (kind, subject)" index for every kind.

DELETE FROM job WHERE kind IN ('enrichment', 'symbology_backfill');

DROP INDEX IF EXISTS uq_job_active_universe_writer;
DROP INDEX IF EXISTS uq_job_active_per_subject;

CREATE UNIQUE INDEX uq_job_active_per_subject
    ON job (kind, subject)
    WHERE status IN ('queued', 'running');
