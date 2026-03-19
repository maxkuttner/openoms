-- Down migration for OMS schema.
-- NOTE: This is intentionally kept in sync with `database/migrations/000001_init.down.sql`.

DROP TABLE IF EXISTS position_projection_log;
DROP TABLE IF EXISTS order_event_store;
DROP TABLE IF EXISTS order_fills;
DROP TABLE IF EXISTS order_events;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS cash_balance;
DROP TABLE IF EXISTS portfolio_position;
DROP TABLE IF EXISTS provider_instrument;
DROP TABLE IF EXISTS instrument;
DROP TABLE IF EXISTS connector;
DROP TABLE IF EXISTS provider;
DROP TABLE IF EXISTS account;
DROP TABLE IF EXISTS "user";
