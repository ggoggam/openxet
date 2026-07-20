-- Record when each SigV4 credential was minted, so the management UI can show
-- and sort credentials by age. Additive column with a 0 default so rows created
-- before this migration (which had no timestamp) sort last.
--
-- Never edit this file once it has run anywhere — sqlx checksums applied
-- migrations. Schema changes go in new files.

ALTER TABLE s3_credentials ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
