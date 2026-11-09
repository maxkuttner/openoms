-- Inventory reconciliation. The runtime reports confirmed shortfalls of venue
-- balances against its expected inventory (one per line, NULL when clean);
-- an operator REBASELINE command approves the venue's current balances.

ALTER TABLE colo_deployment
    ADD COLUMN inventory_shortfall    TEXT,
    ADD COLUMN inventory_reported_at  TIMESTAMPTZ;

ALTER TABLE colo_deployment_command
    DROP CONSTRAINT colo_deployment_command_kind_check,
    ADD CONSTRAINT colo_deployment_command_kind_check
        CHECK (kind IN ('KILL', 'ENABLE', 'REBASELINE'));
