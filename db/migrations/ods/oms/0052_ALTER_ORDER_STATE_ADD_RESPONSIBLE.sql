-- For a strategy's order, the person responsible for that strategy at the time
-- the order was placed. NULL for orders a person placed themselves.

ALTER TABLE order_state
    ADD COLUMN responsible_principal_id UUID REFERENCES principal(id);
