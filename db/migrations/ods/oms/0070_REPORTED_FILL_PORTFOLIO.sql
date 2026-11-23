-- An explicit admin rebook (adopting a break into another portfolio) pins the
-- fill there; NULL means the strategy's own portfolio. Replay uses
-- COALESCE(reported_fill.portfolio_id, strategy.portfolio_id).
ALTER TABLE reported_fill ADD COLUMN portfolio_id UUID REFERENCES portfolio(id);
