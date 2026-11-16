-- Principals are now people (HUMAN) or algorithms (STRATEGY), and a person can
-- be INVITED before first sign-in.
--
-- SERVICE and DESK go away: an unused one is deleted; one that is used (API
-- key, grant, order, session, watchlist, strategy, identity) becomes a key-only
-- HUMAN user with role `trader`. An admin can disable leftovers from Users.

DELETE FROM principal p
 WHERE p.principal_type IN ('SERVICE', 'DESK')
   AND NOT EXISTS (SELECT 1 FROM api_key a WHERE a.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM principal_portfolio_grant g WHERE g.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM order_state o WHERE o.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM user_session s WHERE s.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM watchlist_item w WHERE w.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM strategy st
                    WHERE st.owner_principal_id = p.id OR st.principal_id = p.id)
   AND NOT EXISTS (SELECT 1 FROM principal_identity i WHERE i.principal_id = p.id);

INSERT INTO principal_role (principal_id, role_code)
SELECT id, 'trader' FROM principal WHERE principal_type IN ('SERVICE', 'DESK');

UPDATE principal SET principal_type = 'HUMAN', updated_at = now()
 WHERE principal_type IN ('SERVICE', 'DESK');

ALTER TABLE principal DROP CONSTRAINT principal_principal_type_check;
ALTER TABLE principal ADD CONSTRAINT principal_principal_type_check
    CHECK (principal_type IN ('HUMAN', 'STRATEGY'));

ALTER TABLE principal DROP CONSTRAINT principal_status_check;
ALTER TABLE principal ADD CONSTRAINT principal_status_check
    CHECK (status IN ('ACTIVE', 'INVITED', 'DISABLED'));

-- A strategy's responsible user is a person, never another strategy. A CHECK
-- cannot read another table, so this is a trigger.
CREATE FUNCTION strategy_owner_not_strategy() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM principal
                WHERE id = NEW.owner_principal_id AND principal_type = 'STRATEGY') THEN
        RAISE EXCEPTION 'strategy % owner % is a STRATEGY principal',
            NEW.code, NEW.owner_principal_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER strategy_owner_not_strategy
    BEFORE INSERT OR UPDATE OF owner_principal_id ON strategy
    FOR EACH ROW EXECUTE FUNCTION strategy_owner_not_strategy();
