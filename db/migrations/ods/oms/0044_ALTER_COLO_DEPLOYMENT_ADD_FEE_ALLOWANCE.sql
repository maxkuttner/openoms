-- Fee headroom the runtime holds back on every buy, in basis points of the
-- order's notional, so it never spends funds the fee will need.

ALTER TABLE colo_deployment
    ADD COLUMN allocation_fee_allowance_bps NUMERIC NOT NULL DEFAULT 0
        CHECK (allocation_fee_allowance_bps >= 0 AND allocation_fee_allowance_bps <= 1000);
