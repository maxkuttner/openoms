ALTER TABLE oms_account
    ADD COLUMN environment TEXT NOT NULL DEFAULT 'PAPER'
        CHECK (environment IN ('PAPER', 'LIVE'));

-- Replace the existing unique constraint with one that includes environment.
-- An external account ref is unique per broker + environment combination.
ALTER TABLE oms_account
    DROP CONSTRAINT oms_account_broker_code_external_account_ref_key;

ALTER TABLE oms_account
    ADD CONSTRAINT oms_account_broker_env_ref_key
        UNIQUE (broker_code, environment, external_account_ref);
