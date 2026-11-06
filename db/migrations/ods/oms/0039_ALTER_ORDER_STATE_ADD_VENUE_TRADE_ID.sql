-- An external order (a co-located strategy's fill) is one order per venue
-- trade. Its trade id is kept so a follow stream's gap is filled from the trade
-- history after the last trade booked.

ALTER TABLE order_state ADD COLUMN venue_trade_id BIGINT;
CREATE INDEX order_state_external_trades ON order_state (portfolio_id, instrument_id, venue_trade_id) WHERE origin = 'EXTERNAL';
