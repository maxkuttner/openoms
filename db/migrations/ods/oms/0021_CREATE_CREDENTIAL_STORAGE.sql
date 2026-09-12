-- Credentials move out of the environment and into the database, sealed with the
-- master key from oms.toml. The database never sees plaintext, and a dump without
-- that key yields nothing usable.
--
-- One opaque blob per connection rather than a column per field: the shapes differ
-- per broker (key/secret for a REST API, host/port/comp-ids/private-key for a FIX
-- session), and serde already models that. The fields worth querying — broker,
-- environment, status — are already columns here.
--
-- Nullable on purpose: a connection may exist unconfigured, which is exactly what
-- the cockpit shows as "needs setup".

ALTER TABLE broker_connection
    ADD COLUMN credentials            BYTEA,
    ADD COLUMN credentials_updated_at TIMESTAMPTZ,
    ADD COLUMN credentials_updated_by TEXT;   -- reserved; null until user accounts exist

COMMENT ON COLUMN broker_connection.credentials IS
    'AES-256-GCM sealed JSON: [12-byte nonce][ciphertext||tag]. AAD is this row''s code.';

-- Feeds are not brokers — a market-data provider has no orders, no accounts and no
-- routing — so they get their own table rather than a nullable-everything column
-- set on broker_connection.
CREATE TABLE feed_connection (
    code                   TEXT PRIMARY KEY,          -- 'databento-opra'
    provider               TEXT NOT NULL,             -- DATABENTO
    dataset                TEXT,                      -- OPRA.PILLAR
    status                 TEXT NOT NULL DEFAULT 'ACTIVE',
    credentials            BYTEA,
    credentials_updated_at TIMESTAMPTZ,
    credentials_updated_by TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (status IN ('ACTIVE','DISABLED'))
);

COMMENT ON TABLE feed_connection IS
    'Configured market-data providers; credentials sealed with the oms.toml master key.';
