-- Initial OMS schema: minimal event store only.

CREATE TABLE order_event (
    gloabl_position BIGSERIAL PRIMARY KEY, -- primary key which determines the ordering of the event log table
    order_id UUID NOT NULL,
    version BIGINT NOT NULL,
    event_id UUID NOT NULL,
    event_type VARCHAR(50) NOT NULL,
    actor VARCHAR(30) NOT NULL DEFAULT 'oms',
    payload_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    correlation_id UUID,
    causation_id UUID,
    schema_version INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(order_id, version),
    UNIQUE(event_id)
);

CREATE INDEX idx_order_event_order_id ON order_event(order_id);
CREATE INDEX idx_order_event_created_at ON order_event(created_at);

CREATE OR REPLACE FUNCTION order_event_immutable_guard()
RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'order_event is append-only';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER order_event_no_update
BEFORE UPDATE ON order_event
FOR EACH ROW EXECUTE FUNCTION order_event_immutable_guard();

CREATE TRIGGER order_event_no_delete
BEFORE DELETE ON order_event
FOR EACH ROW EXECUTE FUNCTION order_event_immutable_guard();
