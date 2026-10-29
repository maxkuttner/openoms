-- Background jobs: one-off work with a beginning and an end (a broker's
-- instrument universe load, enrichment, symbology backfill, an expiry sweep
-- that changed something). Persisted so the cockpit can show what ran, how it
-- ended and why — including across restarts: a job still queued or running when
-- the OMS stops is marked 'interrupted' at the next start, never silently lost.
CREATE TABLE job (
    id             UUID PRIMARY KEY,
    kind           TEXT NOT NULL,
    -- What the job is about, e.g. a broker connection code. NULL for jobs with
    -- no single subject (an expiry sweep).
    subject        TEXT,
    -- 'system' for work the OMS started itself (boot, schedules), else the
    -- admin action that started it.
    started_by     TEXT NOT NULL,
    params         JSONB NOT NULL DEFAULT '{}',
    status         TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'interrupted')),
    progress_done  BIGINT,
    progress_total BIGINT,
    result         JSONB,
    error          TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at     TIMESTAMPTZ,
    finished_at    TIMESTAMPTZ
);

-- At most one active job per (kind, subject): two concurrent universe loads for
-- one connection would be two writers racing on the same rows. Enforced here,
-- atomically, rather than by a check-then-insert in application code.
CREATE UNIQUE INDEX uq_job_active_per_subject
    ON job (kind, subject)
    WHERE status IN ('queued', 'running');

CREATE INDEX idx_job_created ON job (created_at DESC);
CREATE INDEX idx_job_kind_subject ON job (kind, subject, created_at DESC);

-- A job's own log lines, shown when the job is opened in the cockpit. Capped per
-- job by the application (newest lines kept).
CREATE TABLE job_log (
    id      BIGSERIAL PRIMARY KEY,
    job_id  UUID NOT NULL REFERENCES job(id) ON DELETE CASCADE,
    at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    level   TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE INDEX idx_job_log_job ON job_log (job_id, id);
