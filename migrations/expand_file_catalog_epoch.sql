CREATE TABLE IF NOT EXISTS cellar_schema_extension (
  name TEXT PRIMARY KEY NOT NULL,
  fingerprint TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS file_catalog_epoch (
  project_id TEXT PRIMARY KEY NOT NULL,
  version INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
  FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE
);

INSERT OR IGNORE INTO file_catalog_epoch (project_id, version)
  SELECT id, 0 FROM project;

CREATE TRIGGER IF NOT EXISTS file_catalog_epoch_project_insert
AFTER INSERT ON project
BEGIN
  INSERT INTO file_catalog_epoch (project_id, version) VALUES (NEW.id, 0);
END;

CREATE TRIGGER IF NOT EXISTS file_catalog_epoch_file_insert
AFTER INSERT ON file_entry
BEGIN
  UPDATE file_catalog_epoch SET version = version + 1
   WHERE project_id = NEW.project_id;
END;

CREATE TRIGGER IF NOT EXISTS file_catalog_epoch_file_update_same_project
AFTER UPDATE ON file_entry
WHEN OLD.project_id = NEW.project_id
BEGIN
  UPDATE file_catalog_epoch SET version = version + 1
   WHERE project_id = NEW.project_id;
END;

CREATE TRIGGER IF NOT EXISTS file_catalog_epoch_file_update_old_project
AFTER UPDATE ON file_entry
WHEN OLD.project_id <> NEW.project_id
BEGIN
  UPDATE file_catalog_epoch SET version = version + 1
   WHERE project_id = OLD.project_id;
  UPDATE file_catalog_epoch SET version = version + 1
   WHERE project_id = NEW.project_id;
END;

CREATE TRIGGER IF NOT EXISTS file_catalog_epoch_file_delete
AFTER DELETE ON file_entry
BEGIN
  UPDATE file_catalog_epoch SET version = version + 1
   WHERE project_id = OLD.project_id;
END;

CREATE INDEX IF NOT EXISTS ix_file_list_root
  ON file_entry(
    project_id,
    exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
    id COLLATE BINARY
  )
  WHERE parent_id IS NULL
    AND state IN ('live', 'settling', 'unsupported');

CREATE INDEX IF NOT EXISTS ix_file_list_child
  ON file_entry(
    project_id,
    parent_id,
    exact_name COLLATE WINDOWS_ORDINAL_CI_V1,
    id COLLATE BINARY
  )
  WHERE parent_id IS NOT NULL
    AND state IN ('live', 'settling', 'unsupported');

INSERT OR IGNORE INTO cellar_schema_extension (name, fingerprint)
VALUES ('file_catalog_epoch', 'cellar-file-catalog-epoch-v1');
