CREATE TABLE IF NOT EXISTS upload_finalization (
  upload_id TEXT PRIMARY KEY NOT NULL,
  operation_id TEXT NOT NULL UNIQUE,
  file_entry_id TEXT NOT NULL UNIQUE,
  result_identity BLOB CHECK (
    result_identity IS NULL OR length(result_identity) = 24
  ),
  FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE,
  FOREIGN KEY (operation_id) REFERENCES operation(id) ON DELETE CASCADE
);

INSERT OR IGNORE INTO cellar_schema_extension (name, fingerprint)
VALUES ('upload_finalization', 'cellar-upload-finalization-v1');
