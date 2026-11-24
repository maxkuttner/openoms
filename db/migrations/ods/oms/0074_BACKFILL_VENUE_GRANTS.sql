-- Portfolios that traded before grants existed keep trading: every ACTIVE
-- portfolio account becomes an ACTIVE venue grant, with the same params the
-- Rust normalize_venue produces (account_ref only for IBKR brokers).
INSERT INTO trading_grant (subject_type, subject_id, kind, params)
SELECT 'portfolio', a.portfolio_id, 'venue',
       jsonb_build_object(
         'connection', a.broker_connection_code,
         'account_ref', CASE WHEN bc.broker_code IN ('IBKR', 'IBKR_TWS') THEN to_jsonb(btrim(a.external_account_ref)) ELSE 'null'::jsonb END)
FROM account a
JOIN broker_connection bc ON bc.code = a.broker_connection_code
WHERE a.portfolio_id IS NOT NULL AND a.status = 'ACTIVE'
  AND (bc.broker_code NOT IN ('IBKR', 'IBKR_TWS') OR btrim(a.external_account_ref) <> '')
ON CONFLICT (subject_type, subject_id, kind, params) DO NOTHING;
