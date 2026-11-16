-- Roles as data: a role is a named set of permission strings, and each
-- principal holds exactly one role (v1). The backend checks permissions, never
-- role codes.

CREATE TABLE role (
    code        TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT,
    built_in    BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE role_permission (
    role_code   TEXT NOT NULL REFERENCES role(code) ON DELETE CASCADE,
    permission  TEXT NOT NULL,
    PRIMARY KEY (role_code, permission)
);

CREATE TABLE principal_role (
    principal_id UUID PRIMARY KEY REFERENCES principal(id) ON DELETE CASCADE,
    role_code    TEXT NOT NULL REFERENCES role(code)
);

CREATE INDEX idx_principal_role_role ON principal_role(role_code);

INSERT INTO role (code, name, description, built_in) VALUES
    ('pm',       'PM',       'Portfolio manager: sees and manages every order, limit and strategy', true),
    ('trader',   'Trader',   'Trades and cancels own orders',                                        true),
    ('viewer',   'Viewer',   'Read-only access',                                                     true),
    ('strategy', 'Strategy', 'An algorithm trading through the OMS',                                 true);

INSERT INTO role_permission (role_code, permission)
SELECT 'pm', unnest(ARRAY['orders.view_own', 'orders.view_all', 'orders.create', 'orders.cancel_own',
                          'orders.cancel_any', 'positions.view', 'limits.view', 'limits.manage',
                          'strategies.manage'])
UNION ALL
SELECT 'trader', unnest(ARRAY['orders.view_own', 'orders.create', 'orders.cancel_own',
                              'positions.view', 'limits.view'])
UNION ALL
SELECT 'viewer', unnest(ARRAY['orders.view_own', 'orders.view_all', 'positions.view', 'limits.view'])
UNION ALL
SELECT 'strategy', unnest(ARRAY['orders.view_all', 'orders.create', 'orders.cancel_any',
                                'positions.view', 'limits.view']);

-- Existing people become PMs, so someone can still manage everything after the
-- upgrade; existing strategies keep trading. SERVICE/DESK rows are handled by 0051.
INSERT INTO principal_role (principal_id, role_code)
SELECT id, CASE principal_type WHEN 'STRATEGY' THEN 'strategy' ELSE 'pm' END
  FROM principal
 WHERE principal_type IN ('HUMAN', 'STRATEGY');
