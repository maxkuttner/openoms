-- Default broker account an order routes to when it omits an explicit account_id.
-- Nullable: a portfolio may be created before its account exists, then set later.
-- Added via ALTER because portfolio (0002) is created before account (0004).

ALTER TABLE portfolio
    ADD COLUMN default_account_id UUID REFERENCES account(id);

COMMENT ON COLUMN portfolio.default_account_id IS
    'Account an order routes to when /orders/submit omits account_id (explicit account_id overrides).';
