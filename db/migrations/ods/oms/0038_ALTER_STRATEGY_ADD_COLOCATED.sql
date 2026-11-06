-- Co-located strategies (path ②): they trade the venue directly; the OMS
-- follows their account with its own follow-only key and books their fills as
-- external orders. limits_version counts limit changes, so the cockpit can
-- show whether the strategy acknowledged the limits it runs under.

ALTER TABLE strategy
    ADD COLUMN mode                  TEXT    NOT NULL DEFAULT 'VIA_OMS',
    ADD COLUMN follow_environment    TEXT,
    ADD COLUMN follow_credentials    BYTEA,
    ADD COLUMN dead_man_secs         INTEGER NOT NULL DEFAULT 5,
    ADD COLUMN max_orders_per_sec    INTEGER,
    ADD COLUMN kill_when_unmonitored BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN limits_version        BIGINT  NOT NULL DEFAULT 1,
    ADD CONSTRAINT strategy_mode_check CHECK (mode IN ('VIA_OMS', 'COLOCATED')),
    ADD CONSTRAINT strategy_follow_environment_check CHECK (follow_environment IN ('PAPER', 'LIVE')),
    ADD CONSTRAINT strategy_dead_man_secs_check CHECK (dead_man_secs > 0),
    ADD CONSTRAINT strategy_max_orders_per_sec_check CHECK (max_orders_per_sec IS NULL OR max_orders_per_sec > 0);

-- A co-located strategy's fills are booked as orders the OMS did not place.
ALTER TABLE order_state DROP CONSTRAINT order_state_origin_check;
ALTER TABLE order_state ADD CONSTRAINT order_state_origin_check CHECK (origin IN ('API', 'FIX', 'FLATTEN', 'EXTERNAL'));
