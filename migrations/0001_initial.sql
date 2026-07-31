CREATE TABLE project (
  id TEXT PRIMARY KEY NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK (status IN ('active', 'archived')),
  version INTEGER NOT NULL CHECK (version >= 1),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT
);

CREATE TABLE cellar_schema_metadata (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL
);

INSERT INTO cellar_schema_metadata (key, value) VALUES
  ('filename_collation_name', 'WINDOWS_ORDINAL_CI_V1'),
  ('filename_collation_version', '1');

CREATE TABLE file_entry (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  parent_id TEXT,
  exact_name TEXT NOT NULL COLLATE WINDOWS_ORDINAL_CI_V1,
  kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
  platform_kind TEXT NOT NULL,
  volume_serial BLOB CHECK (
    volume_serial IS NULL OR length(volume_serial) = 8
  ),
  filesystem_file_id BLOB CHECK (
    filesystem_file_id IS NULL OR length(filesystem_file_id) = 16
  ),
  size INTEGER NOT NULL CHECK (size >= 0),
  mtime_filetime_100ns INTEGER NOT NULL,
  hash BLOB CHECK (hash IS NULL OR length(hash) = 32),
  hash_state TEXT NOT NULL CHECK (
    hash_state IN ('unknown', 'queued', 'computing', 'ready', 'failed')
  ),
  state TEXT NOT NULL CHECK (
    state IN ('live', 'settling', 'missing', 'trashed', 'unsupported')
  ),
  revision INTEGER NOT NULL CHECK (revision >= 1),
  scan_generation INTEGER NOT NULL,
  observed_at TEXT NOT NULL,
  UNIQUE (id, project_id),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (parent_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE TABLE project_cover (
  project_id TEXT PRIMARY KEY NOT NULL,
  file_entry_id TEXT NOT NULL,
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (file_entry_id, project_id)
    REFERENCES file_entry(id, project_id)
    ON DELETE CASCADE
);

CREATE TABLE upload_session (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  destination_parent_id TEXT,
  destination_name TEXT NOT NULL COLLATE WINDOWS_ORDINAL_CI_V1,
  expected_size INTEGER NOT NULL CHECK (expected_size >= 0),
  committed_offset INTEGER NOT NULL CHECK (committed_offset >= 0),
  expected_hash BLOB,
  pending_offset INTEGER,
  pending_length INTEGER,
  pending_digest BLOB,
  state TEXT NOT NULL CHECK (
    state IN ('created', 'uploading', 'verifying', 'committing',
              'complete', 'failed', 'cancelled')
  ),
  expires_at TEXT NOT NULL,
  CHECK (committed_offset <= expected_size),
  CHECK (expected_hash IS NULL OR length(expected_hash) = 32),
  CHECK (
    (pending_offset IS NULL AND pending_length IS NULL AND pending_digest IS NULL)
    OR
    (pending_offset IS NOT NULL
     AND pending_length IS NOT NULL
     AND pending_digest IS NOT NULL
     AND pending_offset = committed_offset
     AND pending_length > 0
     AND pending_length <= expected_size - pending_offset
     AND length(pending_digest) = 32)
  ),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (destination_parent_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE TABLE operation (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT,
  kind TEXT NOT NULL,
  state TEXT NOT NULL CHECK (
    state IN ('pending', 'fs_applied', 'complete', 'failed')
  ),
  payload_version INTEGER NOT NULL CHECK (payload_version >= 1),
  payload TEXT NOT NULL,
  error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (project_id) REFERENCES project(id)
);

CREATE TABLE trash_item (
  id TEXT PRIMARY KEY NOT NULL,
  project_id TEXT NOT NULL,
  root_entry_id TEXT,
  original_path_snapshot TEXT NOT NULL,
  storage_path TEXT NOT NULL UNIQUE,
  deleted_at TEXT NOT NULL,
  purge_after TEXT NOT NULL,
  state TEXT NOT NULL CHECK (
    state IN ('stored', 'restoring', 'restored', 'purging', 'purged', 'failed')
  ),
  FOREIGN KEY (project_id) REFERENCES project(id),
  FOREIGN KEY (root_entry_id, project_id)
    REFERENCES file_entry(id, project_id)
);

CREATE TABLE audit_event (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  operation_id TEXT,
  source TEXT NOT NULL CHECK (
    source IN ('web', 'explorer', 'reconcile', 'system')
  ),
  project_id TEXT,
  target_id TEXT,
  path_snapshot TEXT,
  action TEXT NOT NULL,
  result TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  details TEXT NOT NULL,
  FOREIGN KEY (operation_id) REFERENCES operation(id),
  FOREIGN KEY (project_id) REFERENCES project(id)
);
