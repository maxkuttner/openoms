-- Groups (the Open WebUI model) assign people's portfolio rights: a group has
-- members and, per portfolio, an access level. A person's access on a
-- portfolio is the highest level across their groups: manage (PM
-- permissions) includes trade (trader permissions) includes view (viewer
-- permissions). Strategies keep their direct principal_portfolio_grant rows
-- and their `strategy` role.

CREATE TABLE user_group (
    id          UUID PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uq_user_group_name ON user_group (name);

CREATE TABLE user_group_member (
    group_id     UUID NOT NULL REFERENCES user_group(id) ON DELETE CASCADE,
    principal_id UUID NOT NULL REFERENCES principal(id),
    PRIMARY KEY (group_id, principal_id)
);
CREATE INDEX idx_user_group_member_principal ON user_group_member(principal_id);

CREATE TABLE user_group_portfolio (
    group_id     UUID NOT NULL REFERENCES user_group(id) ON DELETE CASCADE,
    portfolio_id UUID NOT NULL REFERENCES portfolio(id),
    access       TEXT NOT NULL CHECK (access IN ('view', 'trade', 'manage')),
    PRIMARY KEY (group_id, portfolio_id)
);

-- Every person's existing grants become one group of their own, so nobody
-- loses access: PM + trade -> manage, trader + trade -> trade, anything else
-- -> view (a view-only PM grant keeps view; its limits/strategy rights need
-- manage, which also allows trading, so it is not widened silently).
CREATE TEMP TABLE migrated_group AS
SELECT p.id AS principal_id, md5(random()::text || p.id::text)::uuid AS group_id, p.code
  FROM principal p
 WHERE p.principal_type = 'HUMAN'
   AND EXISTS (SELECT 1 FROM principal_portfolio_grant g WHERE g.principal_id = p.id);

INSERT INTO user_group (id, name, description)
SELECT group_id, 'user ' || code, 'Created from ' || code || '''s portfolio grants'
  FROM migrated_group;

INSERT INTO user_group_member (group_id, principal_id)
SELECT group_id, principal_id FROM migrated_group;

INSERT INTO user_group_portfolio (group_id, portfolio_id, access)
SELECT m.group_id, g.portfolio_id,
       CASE WHEN g.can_trade AND r.role_code = 'pm' THEN 'manage'
            WHEN g.can_trade AND r.role_code = 'trader' THEN 'trade'
            ELSE 'view' END
  FROM migrated_group m
  JOIN principal_portfolio_grant g ON g.principal_id = m.principal_id
  LEFT JOIN principal_role r ON r.principal_id = m.principal_id
 WHERE g.can_view;

DELETE FROM principal_portfolio_grant g
 USING principal p
 WHERE p.id = g.principal_id AND p.principal_type = 'HUMAN';

DROP TABLE migrated_group;

-- What every check reads: one row per (principal, portfolio, source) with the
-- role whose permissions apply there. Direct grants carry the principal's own
-- role (strategies); group access maps to pm / trader / viewer.
CREATE VIEW portfolio_access AS
SELECT g.principal_id, g.portfolio_id, r.role_code, g.can_view, g.can_trade, g.can_allocate
  FROM principal_portfolio_grant g
  LEFT JOIN principal_role r ON r.principal_id = g.principal_id
UNION ALL
SELECT m.principal_id, gp.portfolio_id,
       CASE max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END)
            WHEN 3 THEN 'pm' WHEN 2 THEN 'trader' ELSE 'viewer' END,
       true,
       max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END) >= 2,
       max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END) >= 2
  FROM user_group_member m
  JOIN user_group_portfolio gp ON gp.group_id = m.group_id
 GROUP BY m.principal_id, gp.portfolio_id;
