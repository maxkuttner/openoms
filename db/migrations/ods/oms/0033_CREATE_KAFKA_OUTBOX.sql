-- Kafka outbox: one row per committed order event not yet acked by Kafka.
--
-- A row per event rather than a "last published global_position" cursor:
-- global_position is assigned at insert, not at commit, so a transaction that
-- took a lower position can commit after one that took a higher position, and a
-- cursor would already have moved past it. A queued row is simply picked up
-- whenever it becomes visible, so nothing is skipped.
--
-- Filled by a trigger in the appending transaction itself: a committed event is
-- always queued and a rolled-back one never is. The bridge deletes a row once
-- every record for that event is acked. Existing events are not backfilled.

CREATE TABLE kafka_outbox (
    global_position BIGINT PRIMARY KEY REFERENCES order_event(global_position),
    queued_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION order_event_enqueue_outbox()
RETURNS trigger AS $$
BEGIN
    INSERT INTO kafka_outbox (global_position) VALUES (NEW.global_position);
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER order_event_outbox
AFTER INSERT ON order_event
FOR EACH ROW EXECUTE FUNCTION order_event_enqueue_outbox();
