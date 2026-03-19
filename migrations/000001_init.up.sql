-- Initial schema for OMS database (oms_db).
-- NOTE: This is intentionally kept in sync with `database/migrations/000001_init.up.sql`.

CREATE TABLE "user" (
    user_id BIGSERIAL PRIMARY KEY,
    username VARCHAR(100) NOT NULL
);

CREATE TABLE account (
    account_id BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL,
    name VARCHAR(50) NOT NULL,
    environment VARCHAR(10) NOT NULL DEFAULT 'paper' CHECK (environment IN ('paper', 'live')),
    base_currency VARCHAR(3) NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    is_active BOOLEAN DEFAULT FALSE,
    FOREIGN KEY (user_id) REFERENCES "user"(user_id) ON DELETE CASCADE
);

CREATE TABLE provider (
    provider_id BIGSERIAL PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    code VARCHAR(3) NOT NULL UNIQUE,
    supports_paper BOOLEAN NOT NULL DEFAULT FALSE,
    supports_live BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE connector (
    connector_id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    provider_id BIGINT NOT NULL,
    environment VARCHAR(10) NOT NULL CHECK (environment IN ('paper', 'live')),
    base_currency VARCHAR(3) NOT NULL,
    external_account_id VARCHAR(100),
    api_key VARCHAR(100),
    connection_status VARCHAR(20) DEFAULT 'untested' CHECK (connection_status IN ('untested', 'valid', 'invalid', 'disabled')),
    last_test_at TIMESTAMP,
    created_on DATE DEFAULT CURRENT_DATE,
    FOREIGN KEY (account_id) REFERENCES account(account_id) ON DELETE CASCADE,
    FOREIGN KEY (provider_id) REFERENCES provider(provider_id) ON DELETE RESTRICT,
    UNIQUE (account_id, provider_id, environment)
);

CREATE TABLE instrument (
    instrument_id BIGSERIAL PRIMARY KEY,
    internal_code VARCHAR(50) UNIQUE,
    name VARCHAR(250) NOT NULL,
    asset_class VARCHAR(20),
    ticker VARCHAR(20),
    isin VARCHAR(12) UNIQUE,
    cusip VARCHAR(9) UNIQUE,
    figi VARCHAR(12) UNIQUE,
    base_currency VARCHAR(3),
    is_active BOOLEAN DEFAULT TRUE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE provider_instrument (
    provider_instrument_id BIGSERIAL PRIMARY KEY,
    provider_id BIGINT NOT NULL,
    instrument_id BIGINT NOT NULL,
    provider_symbol VARCHAR(50) NOT NULL,
    provider_internal_id VARCHAR(100),
    last_sync_at TIMESTAMP,
    FOREIGN KEY (instrument_id) REFERENCES instrument(instrument_id),
    UNIQUE(provider_id, provider_symbol),
    UNIQUE(provider_id, provider_internal_id)
);

CREATE TABLE portfolio_position (
    position_id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    connector_id BIGINT NOT NULL,
    instrument_id BIGINT NOT NULL,
    quantity DECIMAL(20, 8) NOT NULL,
    avg_cost_basis DECIMAL(20, 8) NOT NULL,
    realized_pnl DECIMAL(20, 8) DEFAULT 0,
    last_known_price DECIMAL(20, 8),
    opened_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (account_id) REFERENCES account(account_id) ON DELETE CASCADE,
    FOREIGN KEY (connector_id) REFERENCES connector(connector_id) ON DELETE CASCADE,
    FOREIGN KEY (instrument_id) REFERENCES instrument(instrument_id) ON DELETE RESTRICT,
    UNIQUE(account_id, connector_id, instrument_id)
);

CREATE TABLE cash_balance (
    balance_id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    connector_id BIGINT NOT NULL,
    currency VARCHAR(3) NOT NULL,
    available DECIMAL(20, 8) NOT NULL,
    reserved DECIMAL(20, 8) DEFAULT 0,
    last_synced_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (account_id) REFERENCES account(account_id) ON DELETE CASCADE,
    FOREIGN KEY (connector_id) REFERENCES connector(connector_id) ON DELETE CASCADE,
    UNIQUE(account_id, connector_id, currency)
);

CREATE TABLE orders (
    order_id BIGSERIAL PRIMARY KEY,
    account_id BIGINT NOT NULL,
    connector_id BIGINT NOT NULL,
    instrument_id BIGINT NOT NULL,
    client_order_id VARCHAR(100) NOT NULL,
    side VARCHAR(4) NOT NULL CHECK (side IN ('buy', 'sell')),
    order_type VARCHAR(20) NOT NULL CHECK (order_type IN ('market', 'limit', 'stop', 'stop_limit')),
    quantity DECIMAL(20, 8) NOT NULL,
    limit_price DECIMAL(20, 8),
    filled_quantity DECIMAL(20, 8) NOT NULL DEFAULT 0,
    status VARCHAR(30) NOT NULL CHECK (status IN (
        'received',
        'validated',
        'routed',
        'acknowledged',
        'partially_filled',
        'filled',
        'rejected',
        'cancelled',
        'expired',
        'failed'
    )),
    external_order_id VARCHAR(100),
    error_message TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (account_id) REFERENCES account(account_id) ON DELETE CASCADE,
    FOREIGN KEY (connector_id) REFERENCES connector(connector_id) ON DELETE CASCADE,
    FOREIGN KEY (instrument_id) REFERENCES instrument(instrument_id) ON DELETE RESTRICT,
    UNIQUE(account_id, client_order_id)
);

CREATE TABLE order_events (
    event_id BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL,
    event_type VARCHAR(50) NOT NULL,
    status_after VARCHAR(30) NOT NULL,
    payload_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    source VARCHAR(30) NOT NULL DEFAULT 'oms' CHECK (source IN ('oms', 'alpaca', 'migration', 'seed', 'projection')),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (status_after IN (
        'received',
        'validated',
        'routed',
        'acknowledged',
        'partially_filled',
        'filled',
        'rejected',
        'cancelled',
        'expired',
        'failed'
    ))
);

CREATE TABLE order_fills (
    fill_id BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL,
    provider_fill_id VARCHAR(120),
    fill_quantity DECIMAL(20, 8) NOT NULL,
    fill_price DECIMAL(20, 8) NOT NULL,
    fee_amount DECIMAL(20, 8),
    fee_currency VARCHAR(3),
    executed_at TIMESTAMP NOT NULL,
    raw_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(order_id, provider_fill_id)
);

-- Phase-2 event store (append-only); currently unused by app logic but required by OMS contract checks.
CREATE TABLE order_event_store (
    global_position BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL,
    version INTEGER NOT NULL,
    event_id UUID NOT NULL,
    event_type VARCHAR(50) NOT NULL,
    actor VARCHAR(30) NOT NULL DEFAULT 'oms',
    payload_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    correlation_id UUID,
    causation_id UUID,
    schema_version INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (order_id) REFERENCES orders(order_id) ON DELETE CASCADE,
    UNIQUE(order_id, version),
    UNIQUE(event_id)
);

CREATE TABLE position_projection_log (
    projector_event_id VARCHAR(120) PRIMARY KEY,
    order_id BIGINT NOT NULL,
    applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_account_user_id ON account(user_id);
CREATE UNIQUE INDEX uniq_account_one_active_per_user ON account(user_id) WHERE is_active;
CREATE INDEX idx_connector_account_id ON connector(account_id);
CREATE INDEX idx_connector_provider_id ON connector(provider_id);
CREATE INDEX idx_provider_instrument_provider_id ON provider_instrument(provider_id);
CREATE INDEX idx_provider_instrument_instrument_id ON provider_instrument(instrument_id);
CREATE INDEX idx_portfolio_position_account_id ON portfolio_position(account_id);
CREATE INDEX idx_portfolio_position_connector_id ON portfolio_position(connector_id);
CREATE INDEX idx_cash_balance_account_id ON cash_balance(account_id);
CREATE INDEX idx_cash_balance_connector_id ON cash_balance(connector_id);
CREATE INDEX idx_orders_account_status_created_at ON orders(account_id, status, created_at DESC);
CREATE INDEX idx_orders_connector_created_at ON orders(connector_id, created_at DESC);
CREATE INDEX idx_orders_external_order_id ON orders(external_order_id);
CREATE UNIQUE INDEX idx_orders_connector_external_order_id_unique ON orders(connector_id, external_order_id) WHERE external_order_id IS NOT NULL;
CREATE INDEX idx_orders_instrument_id ON orders(instrument_id);
CREATE INDEX idx_order_events_order_created_at ON order_events(order_id, created_at);
CREATE INDEX idx_order_events_status_created_at ON order_events(status_after, created_at DESC);
CREATE INDEX idx_order_fills_order_executed_at ON order_fills(order_id, executed_at);
CREATE INDEX idx_order_event_store_order_id ON order_event_store(order_id);
CREATE INDEX idx_order_event_store_created_at ON order_event_store(created_at);
CREATE INDEX idx_position_projection_log_order_id ON position_projection_log(order_id);

-- Seed providers
INSERT INTO provider (name, code, supports_paper, supports_live) VALUES
    ('Alpaca', 'ALP', TRUE, TRUE),
    ('Interactive Brokers', 'IBK', FALSE, TRUE),
    ('Binance', 'BIN', TRUE, TRUE),
    ('Coinbase', 'CBP', TRUE, TRUE),
    ('Kraken', 'KRK', TRUE, TRUE)
ON CONFLICT (code) DO NOTHING;

-- Seed test user + account
INSERT INTO "user" (username)
SELECT 'testuser'
WHERE NOT EXISTS (SELECT 1 FROM "user" WHERE username = 'testuser');

INSERT INTO account (user_id, name, environment, base_currency, is_active)
SELECT u.user_id, 'Test Account', 'paper', 'USD', TRUE
FROM "user" u
WHERE u.username = 'testuser'
ON CONFLICT DO NOTHING;

-- Seed instruments
INSERT INTO instrument (ticker, name, base_currency) VALUES
    ('AAPL', 'Apple Inc.', 'USD'),
    ('GOOGL', 'Alphabet Inc.', 'USD'),
    ('TSLA', 'Tesla Inc.', 'USD'),
    ('MSFT', 'Microsoft Corporation', 'USD'),
    ('BTCUSD', 'Bitcoin', 'USD'),
    ('ETHUSD', 'Ethereum', 'USD')
ON CONFLICT DO NOTHING;

-- Seed provider instrument mappings
INSERT INTO provider_instrument (provider_id, instrument_id, provider_symbol)
SELECT p.provider_id, i.instrument_id, i.ticker
FROM provider p
JOIN instrument i ON TRUE
WHERE p.code IN ('ALP', 'BIN')
  AND i.ticker IN ('AAPL', 'GOOGL', 'TSLA', 'MSFT', 'BTCUSD', 'ETHUSD')
ON CONFLICT (provider_id, provider_symbol) DO NOTHING;

-- Seed connector without credentials
INSERT INTO connector (
    account_id,
    provider_id,
    environment,
    base_currency,
    api_key,
    external_account_id,
    connection_status,
    last_test_at
)
SELECT a.account_id, p.provider_id, 'paper', 'USD', NULL, NULL, 'untested', NULL
FROM account a
JOIN provider p ON p.code = 'ALP'
JOIN "user" u ON a.user_id = u.user_id
WHERE u.username = 'testuser'
  AND a.name = 'Test Account'
ON CONFLICT DO NOTHING;
