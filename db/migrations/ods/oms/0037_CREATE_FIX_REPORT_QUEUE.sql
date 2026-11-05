-- Execution reports owed to strategies over FIX, one row per order event on a
-- FIX order. Filled by a trigger in the event's own transaction, so a row
-- exists exactly when its event is committed, whatever order the events'
-- positions commit in (global_position is assigned at insert, not at commit).
-- A row is deleted once its report went out on a logged-on session; what is
-- left is sent at the strategy's next logon. Replaces the high-water cursor,
-- which skipped events that committed after a later position.

CREATE TABLE fix_report_queue (
    strategy_code   TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    global_position BIGINT NOT NULL,
    PRIMARY KEY (strategy_code, global_position)
);

CREATE OR REPLACE FUNCTION order_event_enqueue_fix_report()
RETURNS trigger AS $$
BEGIN
    INSERT INTO fix_report_queue (strategy_code, global_position)
    SELECT s.code, NEW.global_position
    FROM order_state o
    JOIN strategy s ON s.portfolio_id = o.portfolio_id
    WHERE o.order_id = NEW.order_id
      AND o.origin = 'FIX'
      AND NEW.event_type IN ('order_routed', 'order_partially_filled', 'order_filled', 'order_canceled',
                             'order_rejected', 'order_expired', 'cancel_rejected');
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER order_event_fix_report
AFTER INSERT ON order_event
FOR EACH ROW EXECUTE FUNCTION order_event_enqueue_fix_report();

ALTER TABLE strategy DROP COLUMN fix_report_position;
