CREATE TABLE IF NOT EXISTS upload_staging_identity (
  upload_id TEXT PRIMARY KEY NOT NULL,
  platform_identity BLOB NOT NULL CHECK(length(platform_identity) = 24),
  FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE
);

INSERT OR IGNORE INTO cellar_schema_extension (name, fingerprint)
VALUES ('upload_staging_identity', 'cellar-upload-staging-identity-v1');
