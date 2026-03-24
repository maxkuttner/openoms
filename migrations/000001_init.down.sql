DROP TRIGGER IF EXISTS order_event_no_delete ON order_event;
DROP TRIGGER IF EXISTS order_event_no_update ON order_event;
DROP FUNCTION IF EXISTS order_event_immutable_guard();
DROP TABLE IF EXISTS order_event;
