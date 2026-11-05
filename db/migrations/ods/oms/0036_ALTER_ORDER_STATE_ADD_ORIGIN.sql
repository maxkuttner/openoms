-- Where an order came from: the REST API or the cockpit (API), a strategy's FIX
-- session (FIX, with its ClOrdID), or a Flatten. FIX execution reports go only
-- to FIX orders, and Flatten orders survive a strategy's disconnect cancels.

ALTER TABLE order_state
    ADD COLUMN origin TEXT NOT NULL DEFAULT 'API',
    ADD COLUMN fix_cl_ord_id TEXT,
    ADD CONSTRAINT order_state_origin_check CHECK (origin IN ('API', 'FIX', 'FLATTEN')),
    ADD CONSTRAINT order_state_fix_cl_ord_id_unique UNIQUE (portfolio_id, fix_cl_ord_id);
