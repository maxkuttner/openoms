-- Access grants: switches for what a person may use. 'default' rows apply to
-- everyone (subject_id NULL); 'group' rows apply to a group's members.
-- A person's grants are default ∪ every group of theirs.
CREATE TABLE access_grant (
    subject_type text NOT NULL CHECK (subject_type IN ('default', 'group')),
    subject_id   uuid REFERENCES user_group (id) ON DELETE CASCADE,
    grant_key    text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    CHECK ((subject_type = 'default') = (subject_id IS NULL))
);
-- NULL never equals NULL in a unique index, so default rows collapse to the
-- zero uuid: one index covers both subject types.
CREATE UNIQUE INDEX access_grant_subject_key
    ON access_grant (subject_type, (coalesce(subject_id, '00000000-0000-0000-0000-000000000000'::uuid)), grant_key);

-- seed: keep today's behavior (the access_grants tests re-run everything below).
INSERT INTO access_grant (subject_type, subject_id, grant_key)
SELECT 'default', NULL, k
FROM unnest(ARRAY['orders', 'allocate', 'options', 'strategies', 'strategy_tokens', 'limits_view', 'api_keys']) AS k
ON CONFLICT DO NOTHING;

INSERT INTO access_grant (subject_type, subject_id, grant_key)
SELECT 'group', g.group_id, k
FROM (SELECT DISTINCT group_id FROM user_group_portfolio WHERE access = 'manage') g
CROSS JOIN unnest(ARRAY['cancel_any', 'limits_set']) AS k
ON CONFLICT DO NOTHING;
