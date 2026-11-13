-- Small runtime switches an admin can flip without a redeploy.
-- `legacy_admin_enabled` gates the deprecated shared-secret admin login; it
-- starts on so existing installs keep working until named admins exist.

CREATE TABLE system_setting (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO system_setting (key, value) VALUES ('legacy_admin_enabled', 'true'::jsonb);
