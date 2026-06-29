-- One custodian reconciliation run for a broker connection: a point-in-time
-- read-and-compare of the custodian's holdings vs our position projection.

CREATE TABLE recon_run (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    oms_count              INT NOT NULL DEFAULT 0,   -- distinct OMS instruments compared
    custodian_count        INT NOT NULL DEFAULT 0,   -- custodian holdings fetched
    break_count            INT NOT NULL DEFAULT 0,
    started_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at            TIMESTAMPTZ
);

CREATE INDEX idx_recon_run_connection ON recon_run(broker_connection_code, started_at DESC);
