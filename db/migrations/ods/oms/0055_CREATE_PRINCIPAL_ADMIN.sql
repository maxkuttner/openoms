-- Named administrators: a person holds admin rights through a row here rather
-- than through a shared secret. Only people can administer; a STRATEGY
-- principal is rejected. A CHECK cannot read another table, so this is a
-- trigger (same pattern as strategy_owner_not_strategy in 0051).

CREATE TABLE principal_admin (
    principal_id UUID PRIMARY KEY REFERENCES principal(id),
    granted_by   TEXT NOT NULL,
    granted_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE FUNCTION principal_admin_not_strategy() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM principal
                WHERE id = NEW.principal_id AND principal_type = 'STRATEGY') THEN
        RAISE EXCEPTION 'admin principal % is a STRATEGY principal', NEW.principal_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER principal_admin_not_strategy
    BEFORE INSERT OR UPDATE OF principal_id ON principal_admin
    FOR EACH ROW EXECUTE FUNCTION principal_admin_not_strategy();
