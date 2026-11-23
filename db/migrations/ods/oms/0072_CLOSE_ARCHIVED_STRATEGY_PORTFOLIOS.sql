-- Strategies archived before archiving closed their portfolio: close those now.
UPDATE portfolio SET status = 'CLOSED'
 WHERE id IN (SELECT portfolio_id FROM strategy WHERE archived_at IS NOT NULL);
