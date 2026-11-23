-- Position replay reads reported fills per strategy and instrument in trade-time order.
CREATE INDEX idx_reported_fill_replay ON reported_fill (strategy_code, instrument_id, trade_time);
