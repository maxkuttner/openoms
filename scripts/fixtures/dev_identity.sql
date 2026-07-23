-- Dev identity chain: a ready-to-trade principal so a fresh install can place a
-- paper order over the API with no admin setup. Gated by OMS_DEV_IDENTITY — this
-- creates a known API secret and is for PAPER/dev only, never a real deployment.
-- Idempotent via natural keys. All in `oms`, so the app runs it as oms_user.
--
--   HTTP Basic auth:  test-trader-key : test-secret   (PAPER/dev only — not for prod)

-- A broker connection the test account can route through. Real credentialed brokers
-- also get their own connection auto-created (ensure_broker_connections); this one
-- is self-contained so the dev chain works even with no broker creds set.
INSERT INTO oms.broker_connection (code, broker_code, environment, status)
VALUES ('alpaca-paper', 'ALPACA', 'PAPER', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

-- UUID ids are generated, then resolved by code for the FK references
-- (principal/account/portfolio all have UNIQUE code).
INSERT INTO oms.principal (id, code, principal_type, display_name, status)
VALUES (gen_random_uuid(), 'test-trader', 'HUMAN', 'Test Trader', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

-- Custodial account on the alpaca-paper connection. external_account_ref is the
-- broker's own account handle; Alpaca routes on the API key, so it's informational.
INSERT INTO oms.account (id, code, broker_connection_code, external_account_ref, status)
VALUES (gen_random_uuid(), 'test-account', 'alpaca-paper', 'PAPER', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

-- Portfolio whose default route is the test account, so submits need no account_id.
INSERT INTO oms.portfolio (id, code, name, status, default_account_id)
SELECT gen_random_uuid(), 'test-portfolio', 'Test Portfolio', 'ACTIVE', a.id
FROM   oms.account a WHERE a.code = 'test-account'
ON CONFLICT (code) DO NOTHING;

-- Entitle the trader to trade the portfolio.
INSERT INTO oms.principal_portfolio_grant
    (id, principal_id, portfolio_id, can_trade, can_view, can_allocate)
SELECT gen_random_uuid(), p.id, pf.id, true, true, false
FROM   oms.principal p, oms.portfolio pf
WHERE  p.code = 'test-trader' AND pf.code = 'test-portfolio'
ON CONFLICT (principal_id, portfolio_id) DO NOTHING;

-- API key for the trader. key_id 'test-trader-key', secret 'test-secret'.
-- secret_hash is a precomputed bcrypt ($2y$, cost 12) of 'test-secret' — pgcrypto
-- isn't available on the server, and the app's bcrypt::verify accepts $2y$.
INSERT INTO oms.api_key (principal_id, key_id, secret_hash, name)
SELECT p.id, 'test-trader-key',
       '$2y$12$haJ5Dxm/IzRmKITzcrZ.sOvTyt2KCvS7KAEC2OvDaycL1TmojvuSm', 'test-key'
FROM   oms.principal p WHERE p.code = 'test-trader'
ON CONFLICT (key_id) DO NOTHING;
