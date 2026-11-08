-- A co-located deployment owns one physical venue account for one strategy.
-- The account alias is unique per venue/environment so the same exchange
-- account cannot be registered twice under different strategies.

CREATE TABLE colo_deployment (
    id                       UUID PRIMARY KEY,
    strategy_code            TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    account_id               UUID NOT NULL REFERENCES account(id),
    venue                    TEXT NOT NULL,
    environment              TEXT NOT NULL CHECK (environment IN ('PAPER', 'LIVE')),
    external_account_ref     TEXT NOT NULL,
    epoch                    BIGINT NOT NULL DEFAULT 1 CHECK (epoch > 0),
    status                   TEXT NOT NULL DEFAULT 'ACTIVE' CHECK (status IN ('ACTIVE', 'RETIRED')),
    authority_secs           INTEGER NOT NULL CHECK (authority_secs > 0),
    market_data_max_age_secs INTEGER NOT NULL CHECK (market_data_max_age_secs > 0),
    heartbeat_secs           INTEGER NOT NULL DEFAULT 1 CHECK (heartbeat_secs > 0),
    allocation_quote_asset   TEXT NOT NULL,
    allocation_max_notional  NUMERIC NOT NULL CHECK (allocation_max_notional > 0),
    allocation_max_position_qty NUMERIC CHECK (allocation_max_position_qty IS NULL OR allocation_max_position_qty > 0),
    policy_revision          BIGINT NOT NULL DEFAULT 1 CHECK (policy_revision > 0),
    kill_epoch               BIGINT,
    kill_command_id          TEXT,
    verified_stop_at         TIMESTAMPTZ,
    verified_stop_epoch      BIGINT,
    retired_at               TIMESTAMPTZ,
    shutdown_evidence        TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, venue, environment),
    UNIQUE (strategy_code, venue, environment),
    CHECK (verified_stop_epoch IS NULL OR verified_stop_at IS NOT NULL)
);

CREATE TABLE colo_deployment_session (
    deployment_id UUID PRIMARY KEY REFERENCES colo_deployment(id) ON DELETE CASCADE,
    session_nonce UUID NOT NULL,
    acquired_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE colo_deployment_universe (
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    instrument_id BIGINT NOT NULL,
    PRIMARY KEY (deployment_id, instrument_id)
);

-- Explicit per-instrument opening inventory. A base asset held across two
-- pairs is attributed here, never inferred as the same position twice.
CREATE TABLE colo_deployment_baseline (
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    instrument_id BIGINT NOT NULL,
    quantity      NUMERIC NOT NULL,
    cost_basis    NUMERIC NOT NULL DEFAULT 0,
    PRIMARY KEY (deployment_id, instrument_id)
);

CREATE TABLE colo_deployment_asset_baseline (
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    asset         TEXT NOT NULL,
    quantity      NUMERIC NOT NULL CHECK (quantity >= 0),
    PRIMARY KEY (deployment_id, asset)
);

CREATE TABLE colo_deployment_policy (
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    revision      BIGINT NOT NULL,
    payload       JSONB NOT NULL,
    issued_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (deployment_id, revision)
);

CREATE TABLE colo_deployment_command (
    id            UUID PRIMARY KEY,
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    command_id    TEXT NOT NULL UNIQUE,
    kind          TEXT NOT NULL CHECK (kind IN ('KILL', 'ENABLE')),
    epoch         BIGINT NOT NULL,
    prior_epoch   BIGINT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_colo_deployment_strategy ON colo_deployment(strategy_code, created_at DESC);
CREATE INDEX idx_colo_deployment_active ON colo_deployment(strategy_code) WHERE status = 'ACTIVE';
