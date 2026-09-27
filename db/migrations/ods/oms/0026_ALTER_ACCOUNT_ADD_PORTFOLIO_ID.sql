-- A portfolio can now hold more than one account (one per venue it trades
-- through), instead of only the single default_account_id it already has.
-- Nullable: an account not yet assigned to any portfolio simply can't be
-- picked by anyone, the same as every account other than a portfolio's
-- default already implicitly was before this column existed.
ALTER TABLE account ADD COLUMN portfolio_id UUID REFERENCES portfolio(id);

CREATE INDEX idx_account_portfolio ON account(portfolio_id);

-- Backfill: every account that is currently someone's default becomes that
-- portfolio's first account. Accounts that are nobody's default stay NULL —
-- there is no other signal to backfill them from.
UPDATE account a
SET portfolio_id = p.id
FROM portfolio p
WHERE p.default_account_id = a.id;
