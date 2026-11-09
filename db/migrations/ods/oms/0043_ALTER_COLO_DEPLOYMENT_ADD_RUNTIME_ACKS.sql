-- What the co-located runtime has acknowledged. A kill or enable moves
-- through stages (never backwards); the deployment keeps the newest policy
-- revision the runtime installed and the recovery status it last reported.

ALTER TABLE colo_deployment_command
    ADD COLUMN ack_stage TEXT
        CHECK (ack_stage IN ('RECEIVED', 'ADMISSION_STOPPED', 'CANCELS_SENT', 'CONFIRMED_CLEAR')),
    ADD COLUMN acked_at TIMESTAMPTZ,
    ADD CHECK ((ack_stage IS NULL) = (acked_at IS NULL));

ALTER TABLE colo_deployment
    ADD COLUMN acked_policy_revision BIGINT CHECK (acked_policy_revision IS NULL OR acked_policy_revision > 0),
    ADD COLUMN recovery_ready        BOOLEAN,
    ADD COLUMN recovery_missing      TEXT,
    ADD COLUMN recovery_reported_at  TIMESTAMPTZ,
    ADD CHECK ((recovery_ready IS NULL) = (recovery_reported_at IS NULL));
