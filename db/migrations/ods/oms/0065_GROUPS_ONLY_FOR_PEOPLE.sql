-- Groups are the only way a person gets portfolio access.
--
-- 0064 turned a person's grant into a 'view' group row even when the person
-- had no portfolio role, which had meant no permission at all. Remove those
-- rows from the per-person groups 0064 created (description "Created from
-- ..."), so such a grant still gives nothing.
DELETE FROM user_group_portfolio gp
 USING user_group g, user_group_member m, principal p
 WHERE g.id = gp.group_id AND g.description LIKE 'Created from %'
   AND m.group_id = g.id AND p.id = m.principal_id AND p.principal_type = 'HUMAN'
   AND NOT EXISTS (SELECT 1 FROM principal_role r WHERE r.principal_id = p.id);

-- People's leftover portfolio roles (every pre-existing person got 'pm' in
-- 0048) and any direct grants would act outside groups: remove them.
-- Strategies keep their role and direct grants.
DELETE FROM principal_role r USING principal p
 WHERE p.id = r.principal_id AND p.principal_type = 'HUMAN';
DELETE FROM principal_portfolio_grant g USING principal p
 WHERE p.id = g.principal_id AND p.principal_type = 'HUMAN';

-- Note: a migrated grant that had can_allocate = false now allocates. Group
-- access ties allocation to trade rights (trade and manage allocate), as
-- ruled for groups.

-- One row per (principal, portfolio): a direct grant (strategies) wins over
-- group access for the same pair, and UNION removes exact duplicates.
CREATE OR REPLACE VIEW portfolio_access AS
SELECT g.principal_id, g.portfolio_id, r.role_code, g.can_view, g.can_trade, g.can_allocate
  FROM principal_portfolio_grant g
  LEFT JOIN principal_role r ON r.principal_id = g.principal_id
UNION
SELECT m.principal_id, gp.portfolio_id,
       CASE max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END)
            WHEN 3 THEN 'pm' WHEN 2 THEN 'trader' ELSE 'viewer' END,
       true,
       max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END) >= 2,
       max(CASE gp.access WHEN 'manage' THEN 3 WHEN 'trade' THEN 2 ELSE 1 END) >= 2
  FROM user_group_member m
  JOIN user_group_portfolio gp ON gp.group_id = m.group_id
 WHERE NOT EXISTS (SELECT 1 FROM principal_portfolio_grant d
                    WHERE d.principal_id = m.principal_id AND d.portfolio_id = gp.portfolio_id)
 GROUP BY m.principal_id, gp.portfolio_id;
