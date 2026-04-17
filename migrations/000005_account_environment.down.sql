ALTER TABLE oms_account
    DROP CONSTRAINT oms_account_broker_env_ref_key;

ALTER TABLE oms_account
    ADD CONSTRAINT oms_account_broker_code_external_account_ref_key
        UNIQUE (broker_code, external_account_ref);

ALTER TABLE oms_account
    DROP COLUMN environment;
