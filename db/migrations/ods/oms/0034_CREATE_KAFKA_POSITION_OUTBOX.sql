-- Position changes waiting to be published on the positions topic.
--
-- Fills are not the only thing that moves a position: an allocation transfer
-- rewrites two position rows without appending any order event, so positions
-- derived from fill events alone would miss it. A trigger on the projection
-- itself catches every write — fill, transfer, rebuild — in the writing
-- transaction. The bridge publishes the row as it is when it gets there, so
-- several queued changes to one position all carry its latest state.

CREATE TABLE kafka_position_outbox (
    id             BIGSERIAL PRIMARY KEY,
    portfolio_id   UUID NOT NULL,
    instrument_id  TEXT NOT NULL,
    queued_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION position_enqueue_outbox()
RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        INSERT INTO kafka_position_outbox (portfolio_id, instrument_id) VALUES (OLD.portfolio_id, OLD.instrument_id);
    ELSE
        INSERT INTO kafka_position_outbox (portfolio_id, instrument_id) VALUES (NEW.portfolio_id, NEW.instrument_id);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER position_outbox
AFTER INSERT OR UPDATE OR DELETE ON position
FOR EACH ROW EXECUTE FUNCTION position_enqueue_outbox();
