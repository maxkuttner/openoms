-- openoms minimal seed: one tradeable instrument so a fresh install can place a
-- paper order without any data-provider creds. Idempotent (re-runnable).
--
-- Depends on `make db-seed` having loaded the FK targets (currency USD, venue ARCX).
-- instrument.id is GENERATED ALWAYS AS IDENTITY, so we never hardcode it: insert by
-- the (symbol, venue) natural key, then resolve the id for the mappings.

INSERT INTO instrument
    (symbol, venue, currency, asset_class, instrument_class, name,
     price_precision, size_precision, price_increment, size_increment, contract_size)
VALUES
    ('SPY', 'ARCX', 'USD', 'EQUITY', 'SPOT', 'SPDR S&P 500 ETF TRUST',
     2, 0, 0.01, 1, 1)
ON CONFLICT (symbol, venue) DO NOTHING;

-- Symbology cross-reference (oms.instrument_xref): Databento's + Alpaca's view of SPY.
-- native_id is SPY's Alpaca asset id (a global, account-independent UUID), so the
-- order path can route on it.
INSERT INTO oms.instrument_xref
    (instrument_id, source_type, source_code, external_symbol, external_exchange,
     external_native_id, is_tradeable, method, confidence)
SELECT i.id, v.source_type, v.source_code, 'SPY', v.external_exchange, v.native_id,
       v.is_tradeable, 'fixture', 'resolved'
FROM   instrument i
CROSS JOIN (VALUES
    ('PROVIDER', 'DATABENTO', 'ARCX', NULL,                                   NULL::boolean),
    ('BROKER',   'ALPACA',    'ARCA', 'b28f4066-5c6d-479b-a2af-85dc1a8f16fb', true)
) AS v(source_type, source_code, external_exchange, native_id, is_tradeable)
WHERE  i.symbol = 'SPY' AND i.venue = 'ARCX'
ON CONFLICT (source_type, source_code,
             COALESCE(external_symbol, ''), COALESCE(external_exchange, ''))
DO NOTHING;

-- A test broker connection so an account can be created and orders can route.
-- Creds are resolved from env by (broker_code, environment); this is just the
-- routing target an account references. Schema-qualified — it lives in `oms`.
INSERT INTO oms.broker_connection (code, broker_code, environment, status)
VALUES ('alpaca-paper', 'ALPACA', 'PAPER', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

-- --- Test identity chain: a ready-to-trade principal so a fresh install can place
-- a paper order over the API with no admin setup. Idempotent via natural keys.
--
--   HTTP Basic auth:  test-trader-key : test-secret   (PAPER/dev only — not for prod)
--
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
