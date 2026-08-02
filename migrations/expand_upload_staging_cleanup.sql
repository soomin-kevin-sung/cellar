CREATE TABLE IF NOT EXISTS upload_staging_cleanup (
  upload_id TEXT PRIMARY KEY NOT NULL,
  FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE
);

CREATE TRIGGER IF NOT EXISTS upload_staging_cleanup_terminal
AFTER UPDATE OF state ON upload_session
WHEN NEW.state IN ('failed', 'cancelled')
 AND OLD.state NOT IN ('failed', 'cancelled')
BEGIN
  INSERT OR IGNORE INTO upload_staging_cleanup (upload_id) VALUES (NEW.id);
END;

INSERT OR IGNORE INTO upload_staging_cleanup (upload_id)
SELECT id FROM upload_session WHERE state IN ('failed', 'cancelled');

INSERT OR IGNORE INTO cellar_schema_extension (name, fingerprint)
VALUES ('upload_staging_cleanup', 'cellar-upload-staging-cleanup-v1');
