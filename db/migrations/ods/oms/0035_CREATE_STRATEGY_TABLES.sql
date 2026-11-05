-- Strategies / algos that trade through the OMS (path ①).
--
-- A strategy owns its portfolio exclusively: only its service principal trades
-- it. Its limits are the defaults for every instrument; a risk_limits row for
-- (portfolio, instrument) overrides them for that instrument. The FIX password
-- is stored only as a bcrypt hash. fix_report_position is the last order-event
-- global_position reported to the strategy over FIX, from which a Logon replays.

CREATE TABLE strategy (
    code                  TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    portfolio_id          UUID NOT NULL UNIQUE REFERENCES portfolio(id),
    owner_principal_id    UUID NOT NULL REFERENCES principal(id),
    principal_id          UUID NOT NULL UNIQUE REFERENCES principal(id),
    sender_comp_id        TEXT NOT NULL,
    target_comp_id        TEXT NOT NULL,
    fix_password_hash     TEXT,
    allowed_addresses     TEXT[] NOT NULL DEFAULT '{}',
    state                 TEXT NOT NULL DEFAULT 'ACTIVE',
    rest_enabled          BOOLEAN NOT NULL DEFAULT true,
    max_order_quantity    NUMERIC,
    max_order_notional    NUMERIC,
    max_position_quantity NUMERIC,
    max_position_notional NUMERIC,
    price_band_pct        NUMERIC,
    fix_report_position   BIGINT NOT NULL DEFAULT 0,
    last_logon_error      TEXT,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (sender_comp_id, target_comp_id),
    CHECK (state IN ('ACTIVE', 'REDUCING', 'HALTED')),
    CHECK (max_order_quantity    IS NULL OR max_order_quantity    > 0),
    CHECK (max_order_notional    IS NULL OR max_order_notional    > 0),
    CHECK (max_position_quantity IS NULL OR max_position_quantity > 0),
    CHECK (max_position_notional IS NULL OR max_position_notional > 0),
    CHECK (price_band_pct        IS NULL OR price_band_pct        > 0)
);

-- Where a strategy's orders for one broker go: at most one account per broker
-- code, and the account belongs to the strategy's portfolio (checked by the
-- admin API). An order's tag 100 / `venue` names the broker code.
CREATE TABLE strategy_connection (
    strategy_code TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    broker_code   TEXT NOT NULL,
    account_id    UUID NOT NULL REFERENCES account(id),
    PRIMARY KEY (strategy_code, broker_code)
);
