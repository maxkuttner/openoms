-- Co-located strategies are gone. A strategy still marked COLOCATED is
-- archived and halted, then kept as an ordinary strategy row so its orders and
-- fills keep their owner.

UPDATE strategy
   SET archived_at = COALESCE(archived_at, now()), state = 'HALTED', mode = 'VIA_OMS', updated_at = now()
 WHERE mode = 'COLOCATED';

-- Attribution rules a co-located registration managed.
DELETE FROM attribution_rule WHERE owner LIKE 'colo:%';

DROP TABLE IF EXISTS
    colo_baseline_adjustment,
    colo_asset_balance,
    colo_asset_event,
    colo_trade_ledger,
    colo_event_inbox,
    colo_deployment_command,
    colo_deployment_policy,
    colo_deployment_asset_baseline,
    colo_deployment_baseline,
    colo_deployment_universe,
    colo_deployment_session,
    colo_deployment
    CASCADE;

ALTER TABLE strategy DROP CONSTRAINT strategy_mode_check;
ALTER TABLE strategy ADD CONSTRAINT strategy_mode_check CHECK (mode IN ('VIA_OMS', 'REPORTED'));

ALTER TABLE strategy
    DROP COLUMN follow_environment,
    DROP COLUMN follow_credentials,
    DROP COLUMN follow_since,
    DROP COLUMN dead_man_secs,
    DROP COLUMN kill_when_unmonitored,
    DROP COLUMN limits_version;
