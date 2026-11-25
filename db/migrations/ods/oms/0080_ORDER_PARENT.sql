-- Parent/child orders (first user: the futures roll). A roll is one parent order on
-- a calendar-spread instrument, routed to the broker as one combo order, plus one
-- child order per leg that books the leg's fills. A child names its parent here;
-- an ordinary order has none.

ALTER TABLE order_state ADD COLUMN parent_order_id UUID REFERENCES order_state(order_id);

CREATE INDEX order_state_parent_idx ON order_state (parent_order_id) WHERE parent_order_id IS NOT NULL;
