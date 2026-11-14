-- A login identity: one (issuer, subject) pair from an OIDC provider, linked to
-- the principal it signs in as. Every login after an invite claim resolves here;
-- identities are never matched or merged on email.
--
-- Backfill: existing `principal.external_subject` values are linked under the
-- issuer the migrator puts in the `oms.backfill_issuer` session setting
-- (OMS_OIDC_ISSUER_FOR_BACKFILL, else [auth.oidc].issuer). With none set they
-- are left unlinked and their owners re-link through an invite.

CREATE TABLE principal_identity (
    issuer          TEXT NOT NULL,
    subject         TEXT NOT NULL,
    principal_id    UUID NOT NULL REFERENCES principal(id),
    linked_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at   TIMESTAMPTZ,
    PRIMARY KEY (issuer, subject)
);

CREATE INDEX idx_principal_identity_principal ON principal_identity(principal_id);

INSERT INTO principal_identity (issuer, subject, principal_id)
SELECT current_setting('oms.backfill_issuer', true), external_subject, id
  FROM principal
 WHERE external_subject IS NOT NULL
   AND NULLIF(current_setting('oms.backfill_issuer', true), '') IS NOT NULL;

ALTER TABLE principal DROP COLUMN external_subject;
