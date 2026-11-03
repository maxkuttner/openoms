-- A fat-finger cap per broker connection: no single order worth more than
-- max_order_value (quantity × price × contract size, in the instrument's quote
-- currency) is sent through it. NULL means no cap, which only a PAPER
-- connection may have: a LIVE connection always carries one, starting at
-- 1000 (`risk::DEFAULT_LIVE_MAX_ORDER_VALUE`), and an admin can raise or
-- lower it but not remove it.
ALTER TABLE broker_connection ADD COLUMN max_order_value NUMERIC;

UPDATE broker_connection SET max_order_value = 1000 WHERE environment = 'LIVE';

ALTER TABLE broker_connection
    ADD CONSTRAINT broker_connection_max_order_value_positive
        CHECK (max_order_value IS NULL OR max_order_value > 0),
    ADD CONSTRAINT broker_connection_live_has_max_order_value
        CHECK (environment <> 'LIVE' OR max_order_value IS NOT NULL);
