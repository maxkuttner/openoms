-- 0003's table comment says credentials are "resolved from env by (broker_code,
-- environment)" — true when it was written, wrong since 0021 added the
-- `credentials` column: they are sealed in that column now, and the environment
-- is no longer read for them at all (see `oms config import-env`). Migrations are
-- append-only, so 0003 is not edited; this just re-states the comment correctly.

COMMENT ON TABLE broker_connection IS
    'Configured broker+environment routing targets; credentials are sealed in the credentials column (see 0021), not resolved from the environment.';
