-- A universe load and an enrichment for the same broker connection both
-- re-fetch the broker's list and write the same instrument and mapping rows,
-- so they must exclude each other — not just themselves. 0027's index was per
-- (kind, subject), which let one of each run at once: two writers on the same
-- rows. Replace it with one index across both kinds, keyed by subject, and keep
-- per-(kind, subject) exclusion for every other kind.
DROP INDEX uq_job_active_per_subject;

CREATE UNIQUE INDEX uq_job_active_universe_writer
    ON job (subject)
    WHERE status IN ('queued', 'running') AND kind IN ('universe_load', 'enrichment');

-- NULL subjects are distinct in a unique index, so jobs without a subject (an
-- expiry sweep) are not excluded from running alongside each other.
CREATE UNIQUE INDEX uq_job_active_per_subject
    ON job (kind, subject)
    WHERE status IN ('queued', 'running') AND kind NOT IN ('universe_load', 'enrichment');
