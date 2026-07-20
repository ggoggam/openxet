-- Record when each SigV4 credential was minted, so the management UI can show
-- and sort credentials by age. Additive column with a 0 default so rows created
-- before this migration (which had no timestamp) sort last.
--
-- IF NOT EXISTS mirrors the baseline convention so an out-of-band-provisioned
-- database is baselined rather than failing. Never edit this file once it has
-- run anywhere — sqlx checksums applied migrations.

ALTER TABLE s3_credentials ADD COLUMN IF NOT EXISTS created_at BIGINT NOT NULL DEFAULT 0;
