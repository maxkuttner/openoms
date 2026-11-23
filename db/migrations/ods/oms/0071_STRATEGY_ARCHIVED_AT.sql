-- Archiving a reported strategy hides it from the trader's list and stops its
-- reporting (tokens revoked, no new ones), but keeps its fills, batches and
-- portfolio for audit. NULL means not archived.
ALTER TABLE strategy ADD COLUMN archived_at TIMESTAMPTZ;
