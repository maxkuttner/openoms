-- A portfolio's access list can name a single person as well as groups.
-- A person's effective access is the highest across their groups and their
-- own row. Strategies' direct grants still win over both.
CREATE TABLE portfolio_person_access (
    portfolio_id UUID NOT NULL REFERENCES portfolio(id) ON DELETE CASCADE,
    principal_id UUID NOT NULL REFERENCES principal(id) ON DELETE CASCADE,
    access       TEXT NOT NULL CHECK (access IN ('view', 'trade', 'manage')),
    PRIMARY KEY (portfolio_id, principal_id)
);
CREATE INDEX portfolio_person_access_principal_idx ON portfolio_person_access (principal_id);

CREATE OR REPLACE VIEW portfolio_access AS
SELECT g.principal_id, g.portfolio_id, r.role_code, g.can_view, g.can_trade, g.can_allocate
  FROM principal_portfolio_grant g
  LEFT JOIN principal_role r ON r.principal_id = g.principal_id
UNION
SELECT s.principal_id, s.portfolio_id,
       CASE max(s.lvl) WHEN 3 THEN 'pm' WHEN 2 THEN 'trader' ELSE 'viewer' END,
       true,
       max(s.lvl) >= 2,
       max(s.lvl) >= 2
  FROM (SELECT x.principal_id, x.portfolio_id,
               CASE x.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END AS lvl
          FROM (SELECT m.principal_id, gp.portfolio_id, gp.access
                  FROM user_group_member m
                  JOIN user_group_portfolio gp ON gp.group_id = m.group_id
                UNION ALL
                SELECT pa.principal_id, pa.portfolio_id, pa.access
                  FROM portfolio_person_access pa) x) s
 WHERE NOT EXISTS (SELECT 1 FROM principal_portfolio_grant d
                    WHERE d.principal_id = s.principal_id AND d.portfolio_id = s.portfolio_id)
 GROUP BY s.principal_id, s.portfolio_id;
