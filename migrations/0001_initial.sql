CREATE TABLE project (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE upload_session (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  file_name TEXT NOT NULL,
  total_size INTEGER NOT NULL CHECK (total_size >= 0),
  committed_offset INTEGER NOT NULL DEFAULT 0 CHECK (committed_offset >= 0),
  state TEXT NOT NULL CHECK (state IN ('active', 'finalizing', 'complete', 'failed')),
  failure_reason TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX upload_session_project_idx ON upload_session(project_id);
CREATE INDEX upload_session_state_idx ON upload_session(state);
